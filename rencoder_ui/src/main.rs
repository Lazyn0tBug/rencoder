//! rencoder-ui: Dioxus desktop GUI for Rencoder (U21).
//!
//! Entry point — actual UI lives in `app` and state/reducer
//! lives in `msg`. This split keeps each file focused and testable.
//!
//! The previous attempt (Tiers follow-up) used `LaunchBuilder +
//! with_custom_head + asset_dir = "assets"` to load a separate
//! `style.css`. That produced a "Dioxus App" window with a black
//! background — the App() component never rendered. Most likely
//! root cause: my LaunchBuilder call signature was wrong
//! (`with_cfg` doesn't take a `desktop::Config` directly the way I
//! assumed; the asset bundling path is also brittle in cargo-dist
//! packaged binaries). Reverting to the simple `dioxus::launch`
//! and inlining the styles as a `<style>` element inside App()
//! instead. CSS becomes part of the rendered DOM, no asset
//! bundling, no file path resolution at runtime.

mod app;
mod msg;

fn main() {
    dioxus::launch(app::App);
}
