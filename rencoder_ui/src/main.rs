//! rencoder-ui: Dioxus desktop GUI for Rencoder (U21).
//!
//! Entry point — actual UI lives in `app` and state/reducer
//! lives in `msg`. This split keeps each file focused and testable.

mod app;
mod msg;

fn main() {
    dioxus::launch(app::App);
}
