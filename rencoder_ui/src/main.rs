//! rencoder-ui: Dioxus desktop GUI for Rencoder (U21).
//!
//! Entry point — actual UI lives in `app` and state/reducer
//! lives in `msg`. This split keeps each file focused and testable.
//!
//! Logging setup (§3.8.7.5, v1.2 follow-up #4): `tracing_init::init()`
//! runs before `dioxus::launch` so lib's first `tracing::info!`
//! (from `start_controller()` inside App's `use_hook`) flows into
//! tracing — no bridge layer needed. The receiver end is parked
//! in `tracing_init::LOG_RECEIVER` for App's use_hook to clone + drain.
//!
//! **The config file is read before the subscriber goes up**: the log
//! level and the log directory are configuration (design doc §3.10.5),
//! and a level that is only known after tracing starts is no level at
//! all. The read here is **lenient** — the actor loads the same file
//! authoritatively and reports a broken config to the user, so
//! logging never has to be the thing that fails.

mod app;
mod msg;
mod tracing_init;

use std::path::PathBuf;

use rencoder_lib::config::app::{AppConfig, read_config_lenient};
use rencoder_lib::config::paths::{self, Paths};

fn main() {
    // The config is the app's own (design doc §3.6): it sits next to
    // the binary, and it is where the log level lives.
    let config_path = paths::config_file(None);
    let config: AppConfig = config_path
        .as_ref()
        .map(|path| read_config_lenient(path))
        .unwrap_or_default();

    let app_dir = paths::app_dir().unwrap_or_else(|| PathBuf::from("."));
    let paths = Paths::resolve(
        config_path.unwrap_or_else(|| app_dir.join("rencoder.toml")),
        app_dir.clone(),
        // No working directory yet: the run-level state stays with
        // the app until the user picks one.
        app_dir,
        &config,
    );

    tracing_init::init(&paths.log_dir, &config);
    dioxus::launch(app::App);
}
