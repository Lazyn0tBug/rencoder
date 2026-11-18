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

mod app;
mod msg;
mod tracing_init;

fn main() {
    tracing_init::init();
    dioxus::launch(app::App);
}
