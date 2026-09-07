//! CLI entry point.

mod args;
mod interact;
mod log;
mod signal;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    log::init();
    let cli = args::Cli::parse();
    match cli.command {
        args::Command::Encode(encode_args) => encode_cmd(encode_args),
        args::Command::Analyze(analyze_args) => analyze_cmd(analyze_args),
    }
}

/// `smart-encode encode ...` -- Phase 11 will fill in the full pipeline
/// (Config::load, init_encoder_cache, signal::install_handler,
/// collect_files, transcode_batch, per-file progress reporting).
fn encode_cmd(_args: args::EncodeArgs) -> Result<()> {
    Ok(())
}

/// `smart-encode analyze ...` -- Phase 11 will fill in the analyzer
/// pipeline (collect_files, analyze_fps, print/write stats).
fn analyze_cmd(_args: args::AnalyzeArgs) -> Result<()> {
    Ok(())
}
