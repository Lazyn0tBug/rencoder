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
            DirPicker {
                state: state,
                cmd_tx: cmd_tx,
                config: config,
            }
            if let Some(err) = last_error {
                p { class: "error", "Last error: {err}" }
            }
            p { class: "batch", "Batch: {batch_state}" }
            ProgressPanel { state: state }
            LogPanel { state: state }
            p { class: "history", "History: {history_count} records" }
            HistoryPanel { state: state }
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

/// Per-file progress table (v1.1). Reads `state.progress`
/// (HashMap<file_index, ProgressInfo>) and renders one row per
/// active file. Empty when no batch is running.
#[component]
fn ProgressPanel(state: Signal<AppState>) -> Element {
    let progress = state.read().progress.clone();
    if progress.is_empty() {
        return rsx! { p { class: "muted", "(no active progress)" } };
    }
    let mut rows: Vec<(usize, crate::msg::ProgressInfo)> =
        progress.into_iter().collect();
    rows.sort_by_key(|(i, _)| *i);
    rsx! {
        table { class: "progress-table",
            thead { tr {
                th { "File" }
                th { "Frame" }
                th { "FPS" }
                th { "Speed" }
                th { "Time (s)" }
                th { "ETA (s)" }
            } }
            tbody {
                for (idx, info) in rows {
                    tr {
                        td { "{idx}" }
                        td { "{info.frame}" }
                        td { "{info.fps:.1}" }
                        td { "{info.speed:.2}x" }
                        td { "{info.time_secs:.1}" }
                        td { if let Some(e) = info.eta_secs { "{e:.0}" } else { "—" } }
                    }
                }
            }
        }
    }
}

/// History records table (v1.1). Reads `state.history` (loaded
/// via `Command::LoadHistory`). Shows source path, outcome,
/// output path, encoding duration, output size.
#[component]
fn HistoryPanel(state: Signal<AppState>) -> Element {
    let history = state.read().history.clone();
    if history.is_empty() {
        return rsx! { p { class: "muted", "(no history loaded)" } };
    }
    rsx! {
        table { class: "history-table",
            thead { tr {
                th { "Source" }
                th { "Outcome" }
                th { "Output" }
                th { "Duration (s)" }
                th { "Size (B)" }
            } }
            tbody {
                for r in history.iter() {
                    tr {
                        td { "{r.source_path}" }
                        td { "{r.outcome:?}" }
                        td { if let Some(p) = &r.output_path { "{p}" } else { "—" } }
                        td { if let Some(d) = r.encoding_duration_secs { "{d:.1}" } else { "—" } }
                        td { if let Some(s) = r.output_size_bytes { "{s}" } else { "—" } }
                    }
                }
            }
        }
    }
}

/// stderr log ring buffer view (v1.1). Reads `state.log`
/// (VecDeque<LogLine>, capped at 1000 per §3.8.6 / item 30).
/// Shows last 100 lines newest-first.
#[component]
fn LogPanel(state: Signal<AppState>) -> Element {
    let log = state.read().log.clone();
    if log.is_empty() {
        return rsx! { p { class: "muted", "(no log output)" } };
    }
    let lines: Vec<crate::msg::LogLine> = {
        let mut v: Vec<_> = log.iter().rev().take(100).cloned().collect();
        v.reverse();
        v
    };
    let rendered: Vec<(String, usize, String)> = lines
        .iter()
        .map(|l| (l.at.format("%H:%M:%S").to_string(), l.file_index, l.line.clone()))
        .collect();
    rsx! {
        div { class: "log-window",
            for (time, file, msg) in rendered {
                div { class: "log-line",
                    span { class: "time", "{time}" }
                    span { class: "file", "[{file}]" }
                    span { class: "msg", " {msg}" }
                }
            }
        }
    }
}

/// Directory picker (v1.1: plain text input; v1.2 may add native).
/// Reads current state.input_dir for display; on input change,
/// dispatches `Msg::UserSelectedDir` through the reducer (so any
/// future reducer logic on this event — validation, logging,
/// persistence — fires).
///
/// `cmd_tx` and `config` are wrapped in `Signal` so Dioxus's
/// `#[component]` macro can derive `PartialEq` for prop comparison.
#[component]
fn DirPicker(
    state: Signal<AppState>,
    cmd_tx: Signal<Option<Sender<Command>>>,
    config: Signal<std::sync::Arc<AppConfig>>,
) -> Element {
    let current = state.read().input_dir.clone();
    rsx! {
        div { class: "dir-picker",
            label { "Input directory: " }
            if let Some(d) = &current {
                span { class: "current", "{d.display()}" }
            } else {
                span { class: "muted", "(none)" }
            }
            input {
                r#type: "text",
                placeholder: "/path/to/videos",
                value: current.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                oninput: move |e| {
                    let s = e.value();
                    if s.is_empty() {
                        // Empty input → no-op (keep previous selection)
                        return;
                    }
                    let path = std::path::PathBuf::from(&s);
                    // cmd_tx is Option<Sender> (None while actor
                    // initializes); only dispatch if available.
                    if let Some(tx) = cmd_tx.read().clone() {
                        let cfg = config.read().clone();
                        state.with_mut(|st| reduce(st, Msg::UserSelectedDir(path), &tx, &cfg));
                    }
                },
            }
        }
    }
}
