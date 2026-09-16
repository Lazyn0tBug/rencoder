//! Dioxus App component (U21 §3.8.7).
//!
//! Tiers follow-up fix (see git log): the event pump used to
//! block the Dioxus executor (crossbeam_channel::Receiver::recv()
//! is blocking, not a real future). Fix: bridge through a
//! futures::channel::mpsc so the executor can poll without
//! blocking. This file is the full restored App() — all
//! panels + buttons + the original CSS via `include_str!`.

use dioxus::prelude::*;

use std::sync::Arc;

use crossbeam_channel::Sender;
use futures::channel::mpsc;
use futures::StreamExt;

use rencoder_lib::analyzer::records::AnalysisRecord;
use rencoder_lib::config::app::AppConfig;
use rencoder_lib::job::{start_controller, Command, Event};

use crate::msg::{reduce, AppState, Msg};

/// CSS (Tiers follow-up). Inlined as a <style> child of the
/// root div via Dioxus's `dangerous_inner_html` so it's part
/// of the rendered DOM and doesn't need a separate file lookup
/// at runtime.
const CSS: &str = include_str!("../assets/style.css");

#[component]
pub fn App() -> Element {
    let mut state = use_signal(AppState::default);
    let mut cmd_tx: Signal<Option<Sender<Command>>> = use_signal(|| None);
    let mut config: Signal<Arc<AppConfig>> = use_signal(|| Arc::new(AppConfig::default()));

    let mut state_for_coro = state;
    let mut config_for_coro = config;

    use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let handle = start_controller();
        let cfg = handle.config();
        config_for_coro.set(cfg);
        let (tx, evt_rx) = handle.into_split();
        let tx_for_loop = tx.clone();
        cmd_tx.set(Some(tx));

        // Bridge: blocking crossbeam -> futures::mpsc.
        let (mut tx_async, rx_async) = mpsc::unbounded::<Event>();
        std::thread::spawn(move || {
            while let Ok(evt) = evt_rx.recv() {
                if tx_async.unbounded_send(evt).is_err() {
                    break;
                }
            }
        });

        async move {
            let mut stream = rx_async;
            while let Some(evt) = stream.next().await {
                let cfg = config_for_coro.read().clone();
                state_for_coro.with_mut(|s| {
                    reduce(s, Msg::Lib(evt), &tx_for_loop, &cfg)
                });
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
        return rsx! {
            style { dangerous_inner_html: "{CSS}" }
            div { class: "app",
                h1 { "Rencoder" }
                p { "Initializing actor..." }
            }
        };
    };

    let tx_start = tx.clone();
    let tx_cancel = tx.clone();
    let tx_history = tx.clone();
    let tx_preview = tx.clone();
    let tx_save = tx.clone();
    let tx_analyze = tx.clone();

    let mut state_for_start = state;
    let mut state_for_cancel = state;
    let mut state_for_history = state;

    rsx! {
        style { dangerous_inner_html: "{CSS}" }
        div { class: "app",
            h1 { "Rencoder" }
            main {
                DirPicker {
                    state: state,
                    cmd_tx: cmd_tx,
                    config: config,
                }
                if let Some(ref err) = last_error {
                    p { class: "error", "Last error: {err}" }
                }
                p { class: "batch", "Batch: {batch_state}" }
                ProgressPanel { state: state }
                LogPanel { state: state }
                p { class: "history", "History: {history_count} records" }
                HistoryPanel { state: state }
                DryRunPanel { state: state }
                SettingsPanel { config: config }
                AnalyzePanel { state: state }
            }
            div { class: "controls",
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_start.with_mut(|s| reduce(s, Msg::UserClickedStart, &tx_start, &cfg));
                    },
                    "Start"
                }
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_cancel.with_mut(|s| reduce(s, Msg::UserClickedCancel, &tx_cancel, &cfg));
                    },
                    "Cancel"
                }
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_start.with_mut(|s| reduce(s, Msg::UserClickedSaveSettings, &tx_save, &cfg));
                    },
                    "Save Settings"
                }
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_history.with_mut(|s| reduce(s, Msg::UserClickedLoadHistory, &tx_history, &cfg));
                    },
                    "Load History"
                }
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_start.with_mut(|s| reduce(s, Msg::UserClickedPreview, &tx_preview, &cfg));
                    },
                    "Preview (Dry Run)"
                }
                button {
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        state_for_start.with_mut(|s| reduce(s, Msg::UserClickedAnalyze, &tx_analyze, &cfg));
                    },
                    "Analyze"
                }
            }
        }
    }
}

#[component]
fn DirPicker(
    state: Signal<AppState>,
    cmd_tx: Signal<Option<Sender<Command>>>,
    config: Signal<std::sync::Arc<AppConfig>>,
) -> Element {
    let mut local = use_signal(|| {
        state
            .read()
            .input_dir
            .clone()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let state_value = state
        .read()
        .input_dir
        .clone()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if local.read().as_str() != state_value.as_str() {
        local.set(state_value);
    }

    rsx! {
        div { class: "dir-picker",
            label { "Input directory: " }
            if state.read().input_dir.is_some() {
                span { class: "current",
                    "{state.read().input_dir.as_ref().unwrap().display()}" }
            } else {
                span { class: "muted", "(none)" }
            }
            input {
                r#type: "text",
                placeholder: "/path/to/videos",
                value: "{local}",
                oninput: move |e| {
                    let s = e.value();
                    local.set(s.clone());
                    if s.is_empty() { return; }
                    let path = std::path::PathBuf::from(&s);
                    if let Some(tx) = cmd_tx.read().clone() {
                        let cfg = config.read().clone();
                        state.with_mut(|st| {
                            reduce(st, Msg::UserSelectedDir(path), &tx, &cfg)
                        });
                    }
                },
            }
        }
    }
}

#[component]
fn ProgressPanel(state: Signal<AppState>) -> Element {
    let progress = state.read().progress.clone();
    if progress.is_empty() {
        return rsx! { p { class: "muted", "(no active progress)" } };
    }
    let mut rows: Vec<(usize, crate::msg::ProgressInfo)> = progress.into_iter().collect();
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

#[component]
fn DryRunPanel(state: Signal<AppState>) -> Element {
    let results = state.read().dry_run_results.clone();
    if results.is_empty() {
        return rsx! { p { class: "muted", "(no dry-run results yet — click Preview)" } };
    }
    rsx! {
        table { class: "dryrun-table",
            thead { tr {
                th { "Source" }
                th { "Planned outcome" }
                th { "Output" }
            } }
            tbody {
                for r in results.iter() {
                    tr {
                        td { "{r.source_path}" }
                        td { "{r.outcome:?}" }
                        td { if let Some(p) = &r.output_path { "{p}" } else { "—" } }
                    }
                }
            }
        }
    }
}

#[component]
fn SettingsPanel(config: Signal<std::sync::Arc<AppConfig>>) -> Element {
    let cfg = config.read().clone();
    let enc = cfg.encode.clone().unwrap_or_default();
    rsx! {
        div { class: "settings-panel",
            h3 { "Settings (current load)" }
            p { class: "muted", "Loaded from AppConfig (round-trip target of Save Settings)." }
            p { "format: " strong { if let Some(f) = &enc.format { "{f:?}" } else { "default (Av1)" } } }
            p { "crf: " strong { "{enc.crf}" }
                 "  bitrate: " strong { if let Some(b) = enc.bitrate { "{b}" } else { "—" } } }
            p { "extensions: " strong { if let Some(e) = &enc.extensions { "{e:?}" } else { "[] (all)" } } }
            p { "max_fps: " strong { if let Some(m) = enc.max_fps { "{m}" } else { "off" } }
                 "  nv: " strong { if let Some(n) = enc.nv { "{n}" } else { "off" } } }
            p { class: "muted", "Edit values by hand in the TOML file, then click Save Settings to round-trip." }
        }
    }
}

#[component]
fn AnalyzePanel(state: Signal<AppState>) -> Element {
    let s = state.read();
    let progress_text = s.analyze_progress.as_ref().map(|p| {
        let last_hint = match &p.last {
            Some(f) => format!(" — last: {}", f.path.display()),
            None => String::new(),
        };
        format!(
            "Analyzing: {}/{} done ({} failed){}",
            p.done, p.total, p.failed, last_hint
        )
    });
    let (failed, succeeded): (Vec<&AnalysisRecord>, Vec<&AnalysisRecord>) =
        s.analyze_results.iter().partition(|r| r.fps.is_none());

    if s.analyze_results.is_empty() && s.analyze_progress.is_none() {
        return rsx! { p { class: "muted", "(no analyze results yet — click Analyze)" } };
    }

    rsx! {
        div { class: "analyze-panel",
            h3 { "Analyze" }
            if let Some(text) = progress_text {
                p { class: "progress", "{text}" }
            }
            if !s.analyze_results.is_empty() {
                if !failed.is_empty() {
                    h4 { class: "failed-header", "Failed ({failed.len()})" }
                    table { class: "analyze-failed-table",
                        thead { tr {
                            th { "Path" }
                            th { "File bytes" }
                        } }
                        tbody {
                            for r in failed.iter() {
                                tr {
                                    td { "{r.path.display()}" }
                                    td { "{r.file_bytes}" }
                                }
                            }
                        }
                    }
                }
                if !succeeded.is_empty() {
                    p { class: "muted separator", "── succeeded ({succeeded.len()}) ──" }
                    table { class: "analyze-succeeded-table",
                        thead { tr {
                            th { "Path" }
                            th { "FPS" }
                            th { "Codec" }
                            th { "Duration (s)" }
                            th { "Bytes" }
                        } }
                        tbody {
                            for r in succeeded.iter() {
                                tr {
                                    td { "{r.path.display()}" }
                                    td { if let Some(fps) = r.fps { "{fps}" } else { "—" } }
                                    td { if let Some(c) = &r.codec { "{c}" } else { "—" } }
                                    td { if let Some(d) = r.duration_secs { "{d:.1}" } else { "—" } }
                                    td { "{r.file_bytes}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

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
        .map(|l| {
            (
                l.at.format("%H:%M:%S").to_string(),
                l.file_index,
                l.line.clone(),
            )
        })
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
