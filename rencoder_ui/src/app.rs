//! Dioxus App component (U21 §3.8.7).
//!
//! v1 minimum: render AppState as a JSON-ish pre block so we can see
//! events arrive. Real UI components land incrementally.

use dioxus::prelude::*;

use std::sync::Arc;

use crossbeam_channel::Sender;

use rencoder_lib::config::app::AppConfig;
use rencoder_lib::job::{start_controller, Command};

use crate::msg::{reduce, AppState, Msg};

#[component]
pub fn App() -> Element {
    let mut state = use_signal(AppState::default);
    let mut cmd_tx: Signal<Option<Sender<Command>>> = use_signal(|| None);
    // Loaded AppConfig from start_controller — read by reduce() to
    // derive EncodeParams from config.encode.* (no more defaults).
    let mut config: Signal<Arc<AppConfig>> =
        use_signal(|| Arc::new(AppConfig::default()));

    use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let handle = start_controller();
        let cfg = handle.config();
        let (tx, evt_rx) = handle.into_split();
        let tx_for_loop = tx.clone();
        cmd_tx.set(Some(tx));
        config.set(cfg);
        async move {
            while let Ok(evt) = evt_rx.recv() {
                let cfg = config.read().clone();
                state.with_mut(|s| reduce(s, Msg::Lib(evt), &tx_for_loop, &cfg));
            }
        }
    });

    let s = state.read();
    let history_count = s.history.len();
    let batch_state = s
        .batch
        .as_ref()
        .map(|b| format!("#{} ({} of {})", b.id, b.records.len(), b.total_files))
        .unwrap_or_else(|| "idle".to_string());
    let last_error = s.last_error.clone();
    drop(s);

    let tx_holder = cmd_tx.read().clone();
    let Some(tx) = tx_holder else {
        return rsx! { p { "Initializing actor..." } };
    };

    // `Signal` is Copy — passing it into each closure by value
    // produces a shared handle to the same backing store.
    let mut state_for_start = state;
    let mut state_for_cancel = state;
    let mut state_for_history = state;
    let tx_for_start = tx.clone();
    let tx_for_cancel = tx.clone();
    let tx_for_history = tx.clone();

    rsx! {
        div { class: "app",
            h1 { "Rencoder" }
            p { class: "batch", "Batch: {batch_state}" }
            p { class: "history", "History: {history_count} records" }
            if let Some(err) = last_error {
                p { class: "error", "Last error: {err}" }
            }
            div { class: "controls",
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_start.with_mut(|s| reduce(s, Msg::UserClickedStart, &tx_for_start, &cfg));
                    },
                    "Start"
                }
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_cancel.with_mut(|s| reduce(s, Msg::UserClickedCancel, &tx_for_cancel, &cfg));
                    },
                    "Cancel"
                }
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_history.with_mut(|s| reduce(s, Msg::UserClickedLoadHistory, &tx_for_history, &cfg));
                    },
                    "Load History"
                }
            }
        }
    }
}
