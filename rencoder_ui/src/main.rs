//! rencoder-ui: Dioxus desktop GUI for Rencoder (U21).
//!
//! Entry point — actual UI lives in `app` and state/reducer
//! lives in `msg`. This split keeps each file focused and testable.

mod app;
mod msg;

fn main() {
    // Tiers follow-up: load the basic layout CSS so the panels
    // are actually visible. Without this, the default browser
    // layout stacks everything vertically and most panels render
    // below the fold (h1 + buttons only on a typical screen).
    // `asset_dir = "assets"` in Cargo.toml tells dioxus-desktop
    // (wry) to expose `style.css` at the binary's runtime path.
    let head = r#"<link rel="stylesheet" href="style.css">"#.to_string();
    let config = dioxus::desktop::Config::new().with_custom_head(head);
    dioxus::LaunchBuilder::new()
        .with_cfg(config)
        .launch(app::App);
}
