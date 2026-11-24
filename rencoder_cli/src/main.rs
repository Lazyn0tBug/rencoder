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
mod keys;
mod signal;
mod tracing_init;

use anyhow::{Context, Result};
use clap::Parser;
use rencoder_lib::pipeline::cancel::CancelToken;
use rencoder_lib::{
    capability::{Capabilities, MachineFacts},
    collect::collect_files,
    config::{paths::Paths, AnalyzeFormatArg, AppConfig},
    data::extract::probe_video_metadata,
    encoder::nvenc::{self},
    pipeline::context::BatchContext,
    pipeline::{batch::transcode_batch, cache::cache_from_resolution},
    types::{EncodeParams, ProcessOutput, Target},
};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use tracing::{info, instrument, warn};

fn main() -> Result<()> {
    let cli = args::Cli::parse();

    // A panic must not leave the terminal in the mode the key reader
    // put it in -- the user's shell would lose its line editing
    // (design doc §3.11.2).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        keys::restore_terminal();
        default_hook(info);
    }));

    // The signal handler comes **first** (design doc §3.11.2): before
    // the handler is installed, a signal takes the default action --
    // the process dies, the records are lost, the partial output stays.
    // That window must not exist, so the handler is installed before
    // anything else happens. The cancel request's path is filled in
    // once the paths resolve (§3.6).
    let cancel_slot = signal::new_cancel_slot();
    signal::install_handler(&cancel_slot);

    match cli.command {
        args::Command::Encode(encode_args) => encode_cmd(encode_args, cli.detect, &cancel_slot),
        args::Command::Analyze(analyze_args) => analyze_cmd(analyze_args, &cancel_slot),
        args::Command::Detect(_) => detect_cmd(&cancel_slot),
    }
}

/// Leave the terminal as we found it, then leave with `code`.
///
/// `process::exit` skips the destructors, so the key reader's guard
/// never runs on this path -- the terminal is restored explicitly
/// (design doc §3.11.2).
fn exit_with(code: i32) -> ! {
    keys::restore_terminal();
    std::process::exit(code);
}

/// Everything a run needs before it starts: the config, the paths
/// (design doc §3.6), and the cancel channel (design doc §3.11.2).
///
/// The order matters (design doc §3.11.2): the signal handler goes in
/// **before any work whose interruption would lose state** -- the
/// capability probe, the file collection, the encoding. A signal
/// before that takes the default action: the process dies. Nothing is
/// lost, because nothing has been produced yet (the config write is
/// atomic).
///
/// The cancel request is a file: the handler writes it, the batch
/// polls for it, and anyone who can write that file can stop a running
/// batch.
fn prepare(
    config_override: Option<&Path>,
    op_dir: Option<&Path>,
    cancel_slot: &signal::CancelSlot,
) -> Result<(AppConfig, Paths, CancelToken)> {
    let app_dir = rencoder_lib::config::paths::app_dir().unwrap_or_else(|| PathBuf::from("."));
    let config_path =
        rencoder_lib::config::paths::config_file(config_override).ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine config location: no --config, $RENENCODER_CONFIG \
             is unset, and binary directory is unavailable. Pass --config explicitly."
            )
        })?;
    // The config is created on first run; the notice is logged after
    // tracing is up (below), because the log directory comes from the
    // config itself (design doc §3.6).
    let config_existed = config_path.exists();
    let app_config = AppConfig::load(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;
    // No operation directory means the run acts on no directory of
    // its own (`rencoder detect`): its state stays with the app.
    let op_dir = op_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| app_dir.clone());
    let paths = Paths::resolve(config_path, app_dir, op_dir, &app_config);

    tracing_init::init(&paths.log_dir).context("initialize tracing")?;
    if !config_existed {
        info!(
            config_path = %paths.config_file.display(),
            "{}",
            rencoder_lib::config::app::CREATED_CONFIG_NOTICE
        );
    }

    // A leftover request would cancel this run as well.
    let _ = std::fs::remove_file(&paths.cancel_file);
    // Point the handler at the file the batch watches.
    signal::set_cancel_path(cancel_slot, &paths.cancel_file);
    let cancel = CancelToken::watching(paths.cancel_file.clone());
    Ok((app_config, paths, cancel))
}

/// `rencoder detect` -- probe this machine's NVENC capability, store the
/// result next to the config, and print a report.
///
/// The report is the point: "which hardware encoders can this machine
/// actually run" becomes something the user can ask, instead of
/// something they only learn from a fallback warning.
fn detect_cmd(cancel_slot: &signal::CancelSlot) -> Result<()> {
    // The handler is already installed -- `main` does it for every
    // command (design doc §3.11.2). Installing a second one is not
    // just redundant: the handler is global (one per process), so a
    // second registration is an error on Windows (the console
    // handler slot is a `OnceLock`) and a second signal thread on
    // Unix. The slot `main` installed is the one `prepare` points at
    // the cancel file, so the graceful cancel path works here too.
    let (_config, paths, cancel) = prepare(None, None, cancel_slot)?;
    // The probe is part of the startup phase: a cancel must not wait
    // for it (design doc §3.11.2).
    let resolution =
        match nvenc::resolve_and_store_with(Some(&paths.capability_record), true, |args| {
            if cancel.is_cancelled() {
                return Err(rencoder_lib::error::EncodeError::UserCancel);
            }
            rencoder_lib::encoder::nvenc::run_ffmpeg(args)
        }) {
            Ok(resolution) => resolution,
            Err(rencoder_lib::error::EncodeError::UserCancel) => {
                exit_with(130);
            }
            Err(e) => return Err(e).context("NVENC detection failed"),
        };
    print!("{}", nvenc::report(&resolution));
    Ok(())
}

/// Log the capability report (design doc §3.10.7).
///
/// The report is a file the frontends read; a failed check does
/// **not** refuse the start -- it is information: the user is told
/// what cannot be done here and decides.
fn report_capabilities(capabilities: &Capabilities) {
    for line in capabilities.lines() {
        info!("capability: {line}");
    }
    for failure in capabilities.failures() {
        warn!(
            check = failure.name,
            detail = failure.detail.as_deref().unwrap_or_default(),
            "this run cannot do that part"
        );
    }
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
/// the state of a machine better than its owner. The lib pushed the
/// review date when it reported the stale record, so a decline keeps
/// the next advice 30 days out.
fn advise_refresh() -> Result<Refresh> {
    if !std::io::stdin().is_terminal() {
        println!(
            "NVENC 能力记录已超过 {} 天，如需更新请运行 `rencoder detect`。",
            nvenc::REVIEW_INTERVAL_DAYS
        );
        return Ok(Refresh::Skip);
    }
    print!(
        "NVENC 能力记录已超过 {} 天，可能已过期。是否现在检查一次？[y/N] ",
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
fn encode_cmd(
    args: args::EncodeArgs,
    force_detect: bool,
    cancel_slot: &signal::CancelSlot,
) -> Result<()> {
    if args.interactive {
        // The wizard is specified (design doc §"交互模式") but not
        // built -- the placeholder file it lived in was a comment and
        // nothing else. The flag used to be accepted and ignored, so
        // `rencoder encode --interactive` failed with "either --file
        // or --dir must be provided" and the user had no idea why.
        // Say what is actually the case.
        anyhow::bail!("the interactive wizard is not implemented yet -- pass --file or --dir");
    }
    let target = if let Some(file) = &args.file {
        Target::File(file.clone())
    } else if let Some(dir) = &args.dir {
        Target::Dir(dir.clone())
    } else {
        anyhow::bail!("either --file or --dir must be provided");
    };

    // The operation directory: where this run acts (design doc §3.6).
    let op_dir = target.operation_dir();
    // The single-instance guard (design doc §3.11.1.1): one instance
    // per target tree. It is taken right after the target is known --
    // before the config, the collection, the encoding -- so a conflict
    // shows up before this run has done anything.
    // Held in an Option: the cancel path releases it explicitly (the
    // process exits there, and `process::exit` skips the destructors).
    let mut target_guard = Some(
        rencoder_lib::guard::TargetGuard::acquire(&op_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("cannot take the target")?,
    );
    let (app_config, paths, cancel) = prepare(args.config.as_deref(), Some(&op_dir), cancel_slot)?;

    let enc = app_config.encode.as_ref();

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

    // The collection is part of the startup phase, so it takes the
    // cancel token too (design doc §3.11.2): Ctrl-C during a long
    // walk must not wait for the walk.
    let files = match collect_files(&target, &extensions, max_batch_size, &cancel) {
        Ok(files) => files,
        Err(rencoder_lib::error::EncodeError::UserCancel) => {
            // The run never started: release the target and leave
            // with the cancel exit code (130).
            drop(target_guard.take());
            exit_with(130);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "failed to collect files from {}",
                    target_path(&target).display()
                )
            });
        }
    };

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
    // `nv` is a run choice, not a machine fact: it travels with the
    // run's parameters and is combined with the capability when the
    // encoder is selected (design doc §3.10.5).
    params.nv = enable_nv;

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

    // The machine facts: gathered once (design doc §3.10.7). Prefer
    // the stored record (probing costs ~200-500ms per encoder, and
    // the answer barely changes). `--detect` ignores the record and
    // checks the machine once; a record past its review date only
    // gets the user asked about it.
    //
    // The capability is resolved whether or not this run uses NVENC:
    // it is a fact about the machine, and `nv` is this run's choice
    // (design doc §3.10.5). Resolving it costs a file read when the
    // stored record is fresh.
    let mut resolution =
        match nvenc::resolve_and_store_with(Some(&paths.capability_record), force_detect, |args| {
            // The probe is part of the startup phase: a cancel must
            // not wait for it (design doc §3.11.2). The runner is the
            // seam every ffmpeg call goes through, so this is the one
            // place to check.
            if cancel.is_cancelled() {
                return Err(rencoder_lib::error::EncodeError::UserCancel);
            }
            rencoder_lib::encoder::nvenc::run_ffmpeg(args)
        }) {
            Ok(resolution) => resolution,
            Err(rencoder_lib::error::EncodeError::UserCancel) => {
                // Cancelled before the batch started.
                drop(target_guard.take());
                exit_with(130);
            }
            Err(e) => {
                // ffmpeg does not run. The check still reports what it
                // found -- the report is information, not a gate --
                // and then this run fails.
                let capabilities = rencoder_lib::capability::check_run_capabilities(
                    &paths,
                    &MachineFacts::ffmpeg_broken(),
                );
                report_capabilities(&capabilities);
                return Err(e).context("NVENC detection failed");
            }
        };

    // The staleness advisory. The review cycle belongs to the lib: the
    // resolve already pushed the review date when the record was due,
    // so the advisory is shown once and the next run is quiet. Showing
    // it is this front end's part of the contract -- the CLI asks in
    // the terminal, the GUI shows a line with a "check now" button.
    if resolution.review_due && !force_detect {
        match advise_refresh()? {
            Refresh::Yes => {
                resolution = nvenc::resolve_and_store(Some(&paths.capability_record), true)
                    .context("NVENC detection failed")?;
            }
            Refresh::No | Refresh::Skip => {
                // Declined (or not asked in a non-interactive shell):
                // the date is already pushed, so the next run stays
                // quiet for another 30 days.
            }
        }
    }

    // The check: one call, the whole result (design doc §3.10.7). The
    // facts come from the resolution above -- the check adopts them
    // instead of probing the machine a second time, and the report
    // says what this run is actually going to do.
    let facts = MachineFacts::from_resolution(&resolution);
    let capabilities = rencoder_lib::capability::check_run_capabilities(&paths, &facts);
    report_capabilities(&capabilities);

    // `resolve_and_store` already persisted a freshly probed record.
    let cache = cache_from_resolution(&resolution);

    // The signal handler only sets the cancel token: the lib's
    // execution path kills the in-flight ffmpeg, removes its partial
    // output and stops the batch at the next file boundary -- and the
    // records of the files that finished are written below (design doc
    // §3.11.2). The handler itself was installed in `main`, before
    // any of this started.

    // The batch context: this command's parameters, the encoder
    // cache and this machine's NVENC capability (an encoder that
    // fails at runtime is not tried again for the rest of the command).
    let mut ctx = BatchContext::new(params, std::sync::Arc::new(cache), resolution.capability());

    // Ctrl-D: the user asks to quit (design doc §3.11.2). The reader
    // is installed **around the batch** -- the interactive prompt
    // earlier reads whole lines from the same stdin, and two readers
    // on one stdin eat each other's characters.
    let key_reader = keys::install(cancel_slot);
    let report = transcode_batch(&files, &mut ctx, ProcessOutput::Inherit, &cancel, None);
    // The batch is over: the terminal belongs to the user again.
    drop(key_reader);
    let records = report.records;

    // Default output: timestamped Parquet in <app_dir>/data/ (U18; honors
    // $RENENCODER_DATA_DIR). Extension of -o picks the format: .parquet
    // | .jsonl | .csv.
    let output_path = match &args.output {
        Some(p) => p.clone(),
        None => rencoder_lib::data::store::default_encoding_path_in(&paths.records_dir)?,
    };
    rencoder_lib::data::store::write_records(&records, &output_path)
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    eprintln!(
        "Wrote {} record(s) to {}",
        records.len(),
        output_path.display()
    );
    print_encode_summary(&records);

    // The target is released before the exit: `process::exit` skips
    // the destructors, and the user's directory must not keep a
    // leftover marker (design doc §3.11.1.1).
    drop(target_guard.take());

    // An interrupted run is not a successful one: the records are on
    // disk, but the exit code says the batch did not finish (130 =
    // 128 + SIGINT, the shell convention). A deliberate quit (Ctrl-D)
    // is the exception: the user asked for it, so it leaves with 0.
    if matches!(
        report.termination,
        rencoder_lib::pipeline::task::Termination::Cancelled { .. }
    ) {
        exit_with(signal::exit_code_for(true, signal::reason(cancel_slot)));
    }

    Ok(())
}

/// `rencoder analyze ...` -- probes each file, groups by rounded fps,
/// prints summary, optionally writes a detailed file.
#[instrument(skip_all, fields(dir = ?args.dir, file = ?args.file))]
fn analyze_cmd(args: args::AnalyzeArgs, cancel_slot: &signal::CancelSlot) -> Result<()> {
    let target =
        target_from_analyze_args(&args).with_context(|| "failed to determine target directory")?;

    // The operation directory: where this run acts (design doc §3.6).
    let op_dir = target.operation_dir();
    // The single-instance guard's cheap probe (design doc §3.11.1.1):
    // an analyze run writes nothing, so it does not hold the target --
    // but reading a directory another instance is writing gives
    // unreliable numbers, so it refuses instead.
    rencoder_lib::guard::probe(&op_dir)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("cannot analyze this directory")?;
    let (app_config, paths, cancel) = prepare(args.config.as_deref(), Some(&op_dir), cancel_slot)?;
    let an = app_config.analyze.as_ref();

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

    // The collection takes the cancel token (design doc §3.11.2).
    let files = match collect_files(&target, &extensions, max_batch_size, &cancel) {
        Ok(files) => files,
        Err(rencoder_lib::error::EncodeError::UserCancel) => {
            // Cancelled before the analysis started.
            exit_with(130);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "failed to collect files from {}",
                    target_path(&target).display()
                )
            });
        }
    };

    // The check: one call, the whole result (design doc §3.10.7). An
    // analyze run needs no encoder, so nobody resolved the machine's
    // NVENC capability -- the check reads the stored record (no probe)
    // and asks ffmpeg once.
    let capabilities =
        rencoder_lib::capability::check_run_capabilities(&paths, &MachineFacts::default());
    report_capabilities(&capabilities);

    info!("analyzing {} file(s)", files.len());

    let parallel = an.and_then(|a| a.parallel).unwrap_or(false);
    tracing::Span::current().record("parallel", parallel);

    // Ctrl-D: the user asks to quit (design doc §3.11.2) -- the same
    // reader as the encode path, installed around the analysis.
    let key_reader = keys::install(cancel_slot);
    let report = if parallel {
        // The parallel analyzer takes `&[PathBuf]` and bypasses the
        // `InputPath` boundary (probe-only, no argv injection risk).
        // Convert at the CLI boundary; the lib's signature stays
        // untouched per the review #109 plan.
        let paths: Vec<std::path::PathBuf> =
            files.iter().map(|ip| ip.as_path().to_path_buf()).collect();
        let records = rencoder_lib::analyzer::records::analyze_records_parallel(&paths, &cancel)
            .with_context(|| "fps analysis failed")?;
        rencoder_lib::pipeline::task::BatchReport::completed(records)
    } else {
        // U19 step 5: serial analyze routes through TaskManager so the
        // scaffold logs (batch start/end + per-task start/end) are
        // emitted by the manager rather than baked into the probe
        // function. analyze_records_with_probe stays pure: input ->
        // records, no log calls. The parallel variant keeps its own
        // log path because rayon's worker pool has no ordering
        // guarantee.
        let tasks = rencoder_lib::analyzer::records::analyze_tasks(&files);
        let manager = rencoder_lib::pipeline::task::TaskManager::new();
        manager.run(
            &tasks,
            "analyze",
            target_path(&target),
            &cancel,
            |task, _ctx| {
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
            },
        )
    };
    // The analysis is over: the terminal belongs to the user again.
    drop(key_reader);
    let records = &report.records;
    let stats = rencoder_lib::analyzer::records::aggregate_records(records);

    // An interrupted analysis says so with its exit code, like the
    // encode path (design doc §3.11.2). A deliberate quit (Ctrl-D)
    // leaves with 0.
    if matches!(
        report.termination,
        rencoder_lib::pipeline::task::Termination::Cancelled { .. }
    ) {
        exit_with(signal::exit_code_for(true, signal::reason(cancel_slot)));
    }

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
            render(rencoder_lib::analyzer::records::records_to_csv(records))?;
        }
        AnalyzeFormatArg::Table => {
            let df = rencoder_lib::analyzer::records::records_to_dataframe(records)
                .map_err(|e| anyhow::anyhow!("dataframe build: {e}"))?;
            render(format!("{df}"))?;
        }
        AnalyzeFormatArg::Text => {
            render(stats.summary())?;
        }
        AnalyzeFormatArg::Json => {
            render(records_to_jsonl(records))?;
        }
    }

    Ok(())
}

/// Render records as JSON Lines: one `AnalysisRecord` per line.
///
/// The escaping is serde's, not a hand-written `"{}"`: a Windows path
/// contains backslashes (`C:\Users\me\a.mp4`), and interpolating it
/// raw produces **invalid JSON** -- every line with a Windows path in
/// it would fail to parse. The output must be the same on every
/// platform (design doc §1.3).
fn records_to_jsonl(records: &[rencoder_lib::analyzer::records::AnalysisRecord]) -> String {
    let mut out = String::new();
    for r in records {
        let line = serde_json::json!({
            "path": r.path.to_string_lossy(),
            "fps": r.fps,
            "codec": r.codec.clone(),
            "duration_secs": r.duration_secs,
            "file_bytes": r.file_bytes,
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

fn print_encode_summary(results: &[rencoder_lib::types::EncodingRecord]) {
    // The wording is the lib's (design doc §3.10.3); the CLI lays it
    // out.
    println!();
    println!("Summary:");
    for line in rencoder_lib::data::store::summary_lines(results) {
        println!("  {line}");
    }
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
    fn an_interrupted_run_leaves_with_130() {
        assert_eq!(
            signal::exit_code_for(true, Some(signal::Reason::Interrupt)),
            130
        );
    }

    #[test]
    fn a_deliberate_quit_leaves_with_zero() {
        // The user asked for it: the records are on disk, the run
        // ended the way they wanted. The shell sees a success.
        assert_eq!(signal::exit_code_for(true, Some(signal::Reason::Quit)), 0);
    }

    #[test]
    fn a_finished_run_leaves_with_zero() {
        assert_eq!(signal::exit_code_for(false, None), 0);
        assert_eq!(signal::exit_code_for(false, Some(signal::Reason::Quit)), 0);
    }

    #[test]
    fn a_cancel_from_someone_else_keeps_the_conservative_code() {
        // The cancel file was written by a script, not by us: we
        // cannot tell what was meant, so 130 stands.
        assert_eq!(signal::exit_code_for(true, None), 130);
    }

    #[test]
    fn the_jsonl_output_survives_a_windows_path() {
        // The path is escaped, so the line is valid JSON on every
        // platform. Interpolating it raw produced `C:\Users\...`,
        // which is not JSON (a backslash starts an escape).
        let record = rencoder_lib::analyzer::records::AnalysisRecord::new(
            PathBuf::from(r"C:\Users\me\my videos\a.mp4"),
            Some(30),
            Some("h264".into()),
            Some(1.5),
            42,
        );
        let line = records_to_jsonl(std::slice::from_ref(&record));
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim()).expect("the line must be valid JSON");
        assert_eq!(parsed["path"], r"C:\Users\me\my videos\a.mp4");
        assert_eq!(parsed["fps"], 30);
        assert_eq!(parsed["codec"], "h264");
        assert_eq!(parsed["file_bytes"], 42);
    }

    #[test]
    fn the_jsonl_output_marks_a_failed_probe_as_null() {
        let record = rencoder_lib::analyzer::records::AnalysisRecord::new(
            PathBuf::from("/videos/broken.mp4"),
            None,
            None,
            None,
            0,
        );
        let line = records_to_jsonl(std::slice::from_ref(&record));
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim()).expect("the line must be valid JSON");
        assert!(parsed["fps"].is_null());
        assert!(parsed["codec"].is_null());
        assert!(parsed["duration_secs"].is_null());
    }

    #[test]
    fn params_have_no_encoder_override_without_an_encode_section() {
        let params = EncodeParams::from_app_config(None);
        assert!(params.encoders.av1.is_none());
    }

    #[test]
    fn params_pick_up_the_encoder_overrides() {
        let enc = EncodeConfig {
            encoders: Some(EncodersConfig {
                av1: Some("libaom-av1".into()),
                vp9: Some("libvpx-vp9".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let params = EncodeParams::from_app_config(Some(&enc));
        assert_eq!(params.encoders.av1.as_deref(), Some("libaom-av1"));
        assert_eq!(params.encoders.vp9.as_deref(), Some("libvpx-vp9"));
        assert!(params.encoders.x264.is_none());
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

        let cfg = AppConfig::load(&path).expect("auto-create load");
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

        let cfg = AppConfig::load(&path).expect("load existing");
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
