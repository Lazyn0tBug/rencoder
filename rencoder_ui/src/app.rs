//! Dioxus App component (U21 §3.8.7).
//!
//! Strategy 2 (per the architectural review): keep the lib's
//! `crossbeam-channel` boundary intact and drain events on a
//! dedicated `std::thread`. The thread mutates Dioxus `Signal`s
//! directly via `with_mut` / `set`, which Dioxus 0.6 supports
//! across thread boundaries (the signal's underlying storage
//! is `Send + Sync`).
//!
//! ```
//! [Polars/Rayon worker thread] ---send--->  crossbeam evt_tx
//!                                                   |
//!                                                   v
//!                                           crossbeam evt_rx
//!                                                   |
//!                                                   v
//!                                         [std::thread::spawn]
//!                                         blocks on rx.recv()
//!                                         calls state.with_mut
//!                                         (triggers Dioxus re-render)
//! ```
//!
//! No `use_coroutine`, no `async`, no `futures` crate, no
//! intermediate mpsc bridge. The executor is never blocked
//! because the blocking recv runs on a dedicated OS thread,
//! not in Dioxus's task scheduler.
//!
//! **UI structure** (§3.8.7.2): 4 pages with TopBar tabs + Exit,
//! per-page button in top-right, footer log strip across all
//! pages. Page routing via `state.active_page: Page` (testable
//! through the reducer). No auto-switch between Setup and Task
//! pages — user navigates manually.

use dioxus::desktop;
use dioxus::prelude::*;

use std::sync::Arc;

use crossbeam_channel::Sender;

use rencoder_lib::config::app::AppConfig;
use rencoder_lib::job::{start_controller, Command};

use crate::msg::{reduce, AppState, BatchPhase, Msg, Page, TaskId, TaskQueue};
use crate::tracing_init;

/// CSS inlined as a `<style>` child of the root div via Dioxus's
/// `dangerous_inner_html` so it's part of the rendered DOM and
/// doesn't need a separate file lookup at runtime.
const CSS: &str = include_str!("../assets/style.css");

#[component]
pub fn App() -> Element {
    let state: Signal<AppState, SyncStorage> = use_signal_sync(AppState::default);
    let mut config: Signal<Arc<AppConfig>, SyncStorage> =
        use_signal_sync(|| Arc::new(AppConfig::default()));
    let mut cmd_tx: Signal<Option<Sender<Command>>, SyncStorage> = use_signal_sync(|| None);

    // One-shot setup: start the controller, wire the channels,
    // spawn the event-drain thread. Runs once per App() mount.
    use_hook(|| {
        let handle = start_controller();
        let cfg = handle.config();
        let (tx, evt_rx) = handle.into_split();
        config.set(cfg);
        cmd_tx.set(Some(tx.clone()));

        let mut state_thread = state;
        let config_thread = config;
        let cmd_tx_thread = cmd_tx;
        std::thread::Builder::new()
            .name("rencoder-ui-drain".to_string())
            .spawn(move || {
                while let Ok(evt) = evt_rx.recv() {
                    let cfg = config_thread.read().clone();
                    let tx_for_reduce = cmd_tx_thread.read().clone();
                    if let Some(tx) = tx_for_reduce {
                        state_thread.with_mut(|s| reduce(s, Msg::Lib(evt), &tx, &cfg));
                    }
                }
            })
            .expect("rencoder-ui-drain: failed to spawn event-pump thread");

        // Log drain thread (§3.8.7.5 unified log pipeline):
        // tracing_init owns the Sender side; we cloned the Receiver
        // out of LOG_RECEIVER and capture state for `with_mut`.
        // Each line goes through Msg::LibLogLine -> reducer, which
        // pushes to state.log with source = LogSource::LibInternal.
        if let Some(rx_ref) = tracing_init::LOG_RECEIVER.get() {
            let log_rx = rx_ref.clone();
            let mut state_for_log = state;
            let cmd_tx_for_log = cmd_tx;
            let config_for_log = config;
            std::thread::Builder::new()
                .name("rencoder-ui-log-drain".to_string())
                .spawn(move || {
                    while let Ok(line) = log_rx.recv() {
                        let cfg = config_for_log.read().clone();
                        if let Some(tx) = cmd_tx_for_log.read().clone() {
                            state_for_log.with_mut(|s| reduce(s, Msg::LibLogLine(line), &tx, &cfg));
                        }
                    }
                })
                .expect("rencoder-ui-log-drain: failed to spawn log drain thread");
        }
    });

    // If cmd_tx isn't set yet (use_hook hasn't run), show
    // "Initializing actor..." placeholder. Otherwise render the
    // top-bar + active page.
    let _cmd_tx_ready = cmd_tx.read().is_some();
    if !_cmd_tx_ready {
        return rsx! {
            style { dangerous_inner_html: "{CSS}" }
            div { class: "app",
                header { class: "app-header",
                    span { class: "brand", "RENENCODER" }
                }
                div { class: "page",
                    p { class: "muted", "Initializing actor..." }
                }
            }
        };
    }

    // Page routing — read active_page, render the right page.
    let active_page = state.read().active_page;
    let last_error = state.read().last_error.clone();

    rsx! {
        style { dangerous_inner_html: "{CSS}" }
        div { class: "app",
            TopBar { state: state, cmd_tx: cmd_tx, config: config }
            if let Some(ref err) = last_error {
                div { class: "global-error", "{err}" }
            }
            div { class: "page",
                match active_page {
                    Page::Setup => rsx! { SetupPage { state: state, cmd_tx: cmd_tx, config: config } },
                    Page::Task => rsx! { TaskPage { state: state, cmd_tx: cmd_tx, config: config } },
                    Page::History => rsx! { HistoryPage { state: state, cmd_tx: cmd_tx, config: config } },
                    Page::Log => rsx! { LogPage { state: state } },
                }
            }
            FooterLog { state: state }
        }
    }
}

// ============================================================================
// TopBar — 4 tabs + Exit button (visible on all pages)
// ============================================================================

#[component]
fn TopBar(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Command>>, SyncStorage>,
    config: Signal<Arc<AppConfig>, SyncStorage>,
) -> Element {
    let active = state.read().active_page;
    let tx_exit = cmd_tx.read().clone();
    let tx_tab_setup = cmd_tx.read().clone();
    let tx_tab_task = cmd_tx.read().clone();
    let tx_tab_history = cmd_tx.read().clone();
    let tx_tab_log = cmd_tx.read().clone();

    rsx! {
        header { class: "top-bar",
            span { class: "brand", "RENENCODER" }
            nav { class: "tabs",
                button {
                    class: if active == Page::Setup { "tab tab-active" } else { "tab" },
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        if let Some(tx) = tx_tab_setup.clone() {
                            state.with_mut(|s| reduce(s, Msg::UserClickedTab(Page::Setup), &tx, &cfg));
                        }
                    },
                    "SETUP"
                }
                button {
                    class: if active == Page::Task { "tab tab-active" } else { "tab" },
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        if let Some(tx) = tx_tab_task.clone() {
                            state.with_mut(|s| reduce(s, Msg::UserClickedTab(Page::Task), &tx, &cfg));
                        }
                    },
                    "TASK"
                }
                button {
                    class: if active == Page::History { "tab tab-active" } else { "tab" },
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        if let Some(tx) = tx_tab_history.clone() {
                            state.with_mut(|s| reduce(s, Msg::UserClickedTab(Page::History), &tx, &cfg));
                        }
                    },
                    "HISTORY"
                }
                button {
                    class: if active == Page::Log { "tab tab-active" } else { "tab" },
                    onclick: move |_| {
                        let cfg = config.read().clone();
                        if let Some(tx) = tx_tab_log.clone() {
                            state.with_mut(|s| reduce(s, Msg::UserClickedTab(Page::Log), &tx, &cfg));
                        }
                    },
                    "LOG"
                }
            }
            div { class: "top-bar-right",
                if let Some(tx) = tx_exit {
                    button {
                        class: "exit-btn",
                        onclick: move |_| {
                            // v1 Exit semantics (per §3.8.7.4):
                            // active phase -> Command::Cancel,
                            // then window().close() (after
                            // BatchCancelled in v1.1; v1 close
                            // immediately after dispatch).
                            // None/Done -> close immediately.
                            let phase = state.read().tasks.active.as_ref().map(|t| t.phase);
                            match phase {
                                Some(BatchPhase::Analyzing) | Some(BatchPhase::Encoding) => {
                                    let _ = tx.send(Command::Cancel);
                                }
                                _ => {}
                            }
                            desktop::window().close();
                        },
                        "Exit"
                    }
                }
            }
        }
    }
}

// ============================================================================
// SetupPage — DirPicker + Settings + Start button (top-right)
// ============================================================================

#[component]
fn SetupPage(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Command>>, SyncStorage>,
    config: Signal<Arc<AppConfig>, SyncStorage>,
) -> Element {
    let cfg = config.read().clone();
    let tx_opt = cmd_tx.read().clone();
    let active = state.read().tasks.active.clone();
    let input_dir = state.read().input_dir.clone();

    // Start button enabled iff dir picked AND no active task.
    let can_start = input_dir.is_some() && active.is_none();

    rsx! {
        section { class: "setup-page",
            header { class: "page-header",
                span { class: "section-label", "[ SETUP ]" }
                div { class: "page-header-right",
                    if let Some(tx) = tx_opt {
                        button {
                            class: "primary-btn",
                            disabled: !can_start,
                            onclick: move |_| {
                                let cfg = config.read().clone();
                                state.with_mut(|s| reduce(s, Msg::UserClickedStart, &tx, &cfg));
                            },
                            "Start ▸"
                        }
                    }
                }
            }
            DirPicker { state: state, cmd_tx: cmd_tx, config: config }
            SettingsPanel { config: cfg }
        }
    }
}

#[component]
fn DirPicker(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Command>>, SyncStorage>,
    config: Signal<Arc<AppConfig>, SyncStorage>,
) -> Element {
    let input_dir = state.read().input_dir.clone();
    let active_phase = state.read().tasks.active.as_ref().map(|t| t.phase);
    // Picker disabled unless no active task.
    let enabled = active_phase.is_none() || matches!(active_phase, Some(BatchPhase::Done));

    rsx! {
        div { class: "dir-picker",
            label { class: "section-label", "INPUT DIRECTORY" }
            div { class: "dir-display",
                if let Some(ref d) = input_dir {
                    span { class: "path", "{d.display()}" }
                } else {
                    span { class: "muted", "(none)" }
                }
            }
            button {
                class: "secondary-btn",
                disabled: !enabled,
                onclick: move |_| {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Choose input directory")
                        .pick_folder()
                    {
                        if let Some(tx) = cmd_tx.read().clone() {
                            let cfg = config.read().clone();
                            state.with_mut(|st| {
                                reduce(st, Msg::UserSelectedDir(path), &tx, &cfg)
                            });
                        }
                    }
                },
                "pick directory"
            }
        }
    }
}

#[component]
fn SettingsPanel(config: Arc<AppConfig>) -> Element {
    let enc = config.encode.clone().unwrap_or_default();
    rsx! {
        div { class: "settings-panel",
            label { class: "section-label", "SETTINGS" }
            dl { class: "kv-list",
                dt { "codec" }
                dd { if let Some(f) = &enc.format { "{f:?}" } else { "av1 (default)" } }
                dt { "crf" }
                dd { "{enc.crf}" }
                dt { "bitrate" }
                dd { if let Some(b) = enc.bitrate { "{b}" } else { "—" } }
                dt { "max_fps" }
                dd { if let Some(m) = enc.max_fps { "{m}" } else { "off" } }
                dt { "extensions" }
                dd { if let Some(e) = &enc.extensions { "{e:?}" } else { "default (all)" } }
                dt { "nv" }
                dd { if let Some(n) = enc.nv { "{n}" } else { "off" } }
            }
            p { class: "muted", "Edit values by hand in rencoder.toml, then click Save Settings (v1.2)." }
        }
    }
}

// ============================================================================
// TaskPage — BatchStatus + TaskPlan + Progress + Start/Cancel (contextual)
// ============================================================================

#[component]
fn TaskPage(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Command>>, SyncStorage>,
    config: Signal<Arc<AppConfig>, SyncStorage>,
) -> Element {
    let active = state.read().tasks.active.clone();
    let history = state.read().task_history.clone();
    let progress = state.read().progress.clone();

    rsx! {
        section { class: "task-page",
            header { class: "page-header",
                span { class: "section-label", "[ TASK ]" }
                div { class: "page-header-right",
                    TaskPageButton { state: state, cmd_tx: cmd_tx, config: config }
                }
            }
            if let Some(task) = active.as_ref() {
                BatchStatus {
                    task_id: task.task_id.as_str().to_string(),
                    phase: task.phase,
                    total_files: task.total_files,
                    to_encode: task.to_encode,
                    to_skip: task.to_skip,
                    cancelled: task.cancelled,
                }
                if !task.dry_run_records.is_empty() {
                    div { class: "task-plan-panel",
                        span { class: "section-label", "TASK PLAN" }
                        div { class: "plan-table-wrap",
                            for r in task.dry_run_records.iter() {
                                div { class: "plan-row",
                                    span { class: "path", "{r.source_path}" }
                                    span {
                                        class: match r.outcome {
                                            rencoder_lib::types::EncodingOutcome::Succeeded => "outcome-encode",
                                            rencoder_lib::types::EncodingOutcome::Failed => "outcome-fail",
                                            _ => "outcome-skip",
                                        },
                                        "{r.outcome:?}"
                                    }
                                }
                            }
                        }
                    }
                }
                if !progress.is_empty() {
                    ProgressPanel { state: state }
                }
            } else if let Some(latest) = history.first() {
                // After Done: show last completed task's stats.
                BatchStatus {
                    task_id: latest.task_id.as_str().to_string(),
                    phase: latest.phase,
                    total_files: latest.total_files,
                    to_encode: latest.to_encode,
                    to_skip: latest.to_skip,
                    cancelled: latest.cancelled,
                }
                if !latest.records.is_empty() {
                    p { class: "muted",
                        "Last completed task: {latest.task_id.as_str()} (moved to history)"
                    }
                }
            } else {
                p { class: "muted", "No task running. Pick a directory and click Start." }
            }
        }
    }
}

/// Contextual button on Task page — label + enabled state depend
/// on the active task's phase.
#[component]
fn TaskPageButton(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Command>>, SyncStorage>,
    config: Signal<Arc<AppConfig>, SyncStorage>,
) -> Element {
    let active = state.read().tasks.active.clone();
    let history = state.read().task_history.clone();
    let phase = active.as_ref().map(|t| t.phase);
    let cfg = config.read().clone();
    let tx_opt = cmd_tx.read().clone();
    let Some(tx) = tx_opt else {
        return rsx! { span {} };
    };

    match phase {
        None if !history.is_empty() => {
            // Done state with history: re-arm Start.
            let tx_s = tx.clone();
            let cfg_s = cfg.clone();
            rsx! {
                button {
                    class: "primary-btn",
                    onclick: move |_| {
                        state.with_mut(|s| reduce(s, Msg::UserClickedStart, &tx_s, &cfg_s));
                    },
                    "Start ▸"
                }
            }
        }
        None => rsx! { span {} },
        Some(BatchPhase::Analyzing) => {
            let tx_c = tx.clone();
            rsx! {
                button {
                    class: "secondary-btn",
                    onclick: move |_| {
                        state.with_mut(|s| reduce(s, Msg::UserClickedCancel, &tx_c, &cfg));
                    },
                    "Cancel ✕"
                }
            }
        }
        Some(BatchPhase::AwaitingConfirm) => {
            let active = state.read().tasks.active.clone();
            let to_encode = active.as_ref().map(|t| t.to_encode).unwrap_or(0);
            let tx_s1 = tx.clone();
            let cfg_s1 = cfg.clone();
            let tx_s2 = tx.clone();
            let cfg_s2 = cfg.clone();
            rsx! {
                button {
                    class: "primary-btn",
                    onclick: move |_| {
                        state.with_mut(|s| reduce(s, Msg::UserClickedStart, &tx_s1, &cfg_s1));
                    },
                    "encode {to_encode} files ▸"
                }
                button {
                    class: "secondary-btn",
                    onclick: move |_| {
                        state.with_mut(|s| reduce(s, Msg::UserClickedCancel, &tx_s2, &cfg_s2));
                    },
                    "Cancel ✕"
                }
            }
        }
        Some(BatchPhase::Encoding) => {
            let tx_c = tx.clone();
            rsx! {
                button {
                    class: "secondary-btn",
                    onclick: move |_| {
                        state.with_mut(|s| reduce(s, Msg::UserClickedCancel, &tx_c, &cfg));
                    },
                    "Cancel ✕"
                }
            }
        }
        Some(BatchPhase::Done) => {
            // Active task is Done — shouldn't happen (move to
            // history). Show empty.
            rsx! { span {} }
        }
    }
}

#[component]
fn BatchStatus(
    task_id: String,
    phase: BatchPhase,
    total_files: usize,
    to_encode: usize,
    to_skip: usize,
    cancelled: bool,
) -> Element {
    let badge = match phase {
        BatchPhase::Analyzing => "[● ANALYZING]",
        BatchPhase::AwaitingConfirm => "[● AWAITING]",
        BatchPhase::Encoding => "[● ENCODING]",
        BatchPhase::Done => "[● DONE]",
    };
    let badge_class = match phase {
        BatchPhase::Analyzing | BatchPhase::AwaitingConfirm | BatchPhase::Encoding => {
            "badge badge-active"
        }
        BatchPhase::Done => "badge badge-done",
    };
    rsx! {
        div { class: "batch-status",
            div { class: "batch-status-row",
                span { class: "task-id", "BATCH {task_id}" }
                span { class: "{badge_class}", "{badge}" }
            }
            if total_files > 0 {
                div { class: "batch-status-row",
                    span { class: "code", "{total_files} files" }
                    span { class: "code", "→ {to_encode} to encode" }
                    span { class: "code", "→ {to_skip} skipped" }
                }
            }
            if cancelled {
                p { class: "muted", "cancelled" }
            }
        }
    }
}

#[component]
fn ProgressPanel(state: Signal<AppState, SyncStorage>) -> Element {
    let progress = state.read().progress.clone();
    if progress.is_empty() {
        return rsx! { p { class: "muted", "(no active progress)" } };
    }
    let mut rows: Vec<(usize, crate::msg::ProgressInfo)> = progress.into_iter().collect();
    rows.sort_by_key(|(i, _)| *i);
    rsx! {
        div { class: "progress-panel",
            label { class: "section-label", "PROGRESS" }
            table { class: "progress-table",
                thead { tr {
                        th { "FILE" }
                        th { "FRAME" }
                        th { "FPS" }
                        th { "SPEED" }
                        th { "ETA (s)" }
                } }
                tbody {
                    for (idx, info) in rows {
                        tr {
                            td { "{idx}" }
                            td { "{info.frame}" }
                            td { "{info.fps:.1}" }
                            td { "{info.speed:.2}x" }
                            td { if let Some(e) = info.eta_secs { "{e:.0}" } else { "—" } }
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// HistoryPage — Parquet-loaded records + Refresh button
// ============================================================================

#[component]
fn HistoryPage(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Command>>, SyncStorage>,
    config: Signal<Arc<AppConfig>, SyncStorage>,
) -> Element {
    let history = state.read().history.clone();
    let tx_opt = cmd_tx.read().clone();

    rsx! {
        section { class: "history-page",
            header { class: "page-header",
                span { class: "section-label", "[ HISTORY ]" }
                div { class: "page-header-right",
                    if let Some(tx) = tx_opt {
                        button {
                            class: "secondary-btn",
                            onclick: move |_| {
                                let cfg = config.read().clone();
                                state.with_mut(|s| reduce(s, Msg::UserClickedLoadHistory, &tx, &cfg));
                            },
                            "Refresh ⟳"
                        }
                    }
                }
            }
            if history.is_empty() {
                p { class: "muted", "(no history loaded)" }
            } else {
                HistoryPanel { state: state }
            }
        }
    }
}

#[component]
fn HistoryPanel(state: Signal<AppState, SyncStorage>) -> Element {
    let history = state.read().history.clone();
    let rows: Vec<(String, String, String, String)> = history
        .iter()
        .map(|r| {
            (
                r.started_at.format("%Y-%m-%d %H:%M").to_string(),
                r.source_path.clone(),
                format!("{:?}", r.outcome),
                r.output_size_bytes
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "—".into()),
            )
        })
        .collect();
    rsx! {
        div { class: "history-panel",
            table { class: "history-table",
                thead { tr {
                        th { "WHEN" }
                        th { "SOURCE" }
                        th { "OUTCOME" }
                        th { "SIZE" }
                } }
                tbody {
                    for r in rows.iter() {
                        tr {
                            td { "{r.0}" }
                            td { class: "path", "{r.1}" }
                            td { "{r.2}" }
                            td { "{r.3}" }
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// LogPage — full log scrollback + Clear button
// ============================================================================

#[component]
fn LogPage(state: Signal<AppState, SyncStorage>) -> Element {
    rsx! {
        section { class: "log-page",
            header { class: "page-header",
                span { class: "section-label", "[ LOG ]" }
            }
            LogPanel { state: state }
        }
    }
}

#[component]
fn LogPanel(state: Signal<AppState, SyncStorage>) -> Element {
    let log = state.read().log.clone();
    if log.is_empty() {
        return rsx! { p { class: "muted", "(no log output)" } };
    }
    let lines: Vec<crate::msg::LogLine> = log.iter().rev().take(1000).cloned().collect();
    let rendered: Vec<(String, String, String)> = lines
        .iter()
        .rev()
        .map(|l| {
            // LibInternal lines use file_index = usize::MAX
            // (sentinel). Render as "(global)" instead of a huge
            // number; per-file ffmpeg stderr keeps "[N]".
            let file_label = if l.file_index == crate::msg::NO_FILE_INDEX {
                "global".to_string()
            } else {
                format!("{}", l.file_index)
            };
            (
                l.at.format("%H:%M:%S").to_string(),
                file_label,
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

// ============================================================================
// FooterLog — last 5 lines on every page
// ============================================================================

#[component]
fn FooterLog(state: Signal<AppState, SyncStorage>) -> Element {
    let log = state.read().log.clone();
    let lines: Vec<_> = log.iter().rev().take(5).cloned().collect();
    let rendered: Vec<(String, String, String)> = lines
        .iter()
        .rev()
        .map(|l| {
            let file_label = if l.file_index == crate::msg::NO_FILE_INDEX {
                "global".to_string()
            } else {
                format!("{}", l.file_index)
            };
            (
                l.at.format("%H:%M:%S").to_string(),
                file_label,
                l.line.clone(),
            )
        })
        .collect();
    let empty = rendered.is_empty();
    rsx! {
        footer { class: "log-footer",
            span { class: "section-label", "LOG · last 5 lines" }
            div { class: "log-footer-lines",
                if empty {
                    span { class: "muted", "(no log output)" }
                }
                for entry in rendered.iter() {
                    div { class: "log-line",
                        span { class: "time", "{entry.0}" }
                        span { class: "file", "[{entry.1}]" }
                        span { class: "msg", " {entry.2}" }
                    }
                }
            }
        }
    }
}

// ============================================================================
// Suppress unused-warnings for fields used by future v1.1
// (TaskQueue.pending / AppState.viewing_task used once we lift capacity)
// ============================================================================
#[allow(dead_code)]
fn _unused_field_type_hints(_: TaskQueue, _: TaskId) {}
