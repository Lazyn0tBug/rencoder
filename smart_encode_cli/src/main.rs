//! CLI entry point.

mod args;
mod interact;
mod log;
mod signal;

fn main() -> anyhow::Result<()> {
    println!("smart-encode -- see docs/smart_code.md (Phases 1-2 initialized)");
    Ok(())
}
