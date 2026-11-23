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

use crossbeam_channel::Sender;

use rencoder_lib::config::app::FormatArg;
use rencoder_lib::job::{start_controller, Command, Request};

use crate::msg::{
    reduce, AppState, BatchPhase, ConfigFieldKind, ConfigFieldValue, Msg, Page, TaskId, TaskQueue,
    Toast, ToastKind,
};
use crate::tracing_init;

/// CSS inlined as a `<style>` child of the root div via Dioxus's
/// `dangerous_inner_html` so it's part of the rendered DOM and
/// doesn't need a separate file lookup at runtime.
const CSS: &str = include_str!("../assets/style.css");

#[component]
pub fn App() -> Element {
    let state: Signal<AppState, SyncStorage> = use_signal_sync(AppState::default);
    let mut cmd_tx: Signal<Option<Sender<Request>>, SyncStorage> = use_signal_sync(|| None);

    // One-shot setup: start the controller, wire the channels,
    // spawn the event-drain thread. Runs once per App() mount.
    use_hook(|| {
        let handle = start_controller();
        let cfg = handle.config();
        let (tx, evt_rx) = handle.into_split();
        cmd_tx.set(Some(tx.clone()));

        // v1.2 review #133: seed the Setup-page editor's
        // in-memory draft from the loaded config so the form
        // shows the user's actual values, not `AppConfig::default()`.
        // The form dispatches `UserEditedConfigField`, which the
        // reducer records in the entry layer and fires
        // `Command::SaveConfig` for (design doc §3.10.5).
        let mut state_thread = state;
        state_thread.with_mut(|s| s.config_draft = (*cfg).clone());
        // v1.2 (review #133 follow-up): seed the log with a
        // "controller ready" line so the user sees the log is
        // wired up before any ffmpeg activity. Purely cosmetic —
        // real log entries arrive via Msg::LibLogLine + Event::LogLine.
        state_thread.with_mut(|s| {
            use crate::msg::{LogLine, LogSource, NO_FILE_INDEX};
            s.log.push_back(LogLine {
                file_index: NO_FILE_INDEX,
                source: LogSource::LibInternal,
                line: format!("app started at {}", chrono::Local::now().format("%H:%M:%S")),
                at: chrono::Utc::now(),
            });
        });
        // v1.2 (review #133 follow-up): auto-dispatch
        // Command::LoadHistory on app start so the History tab
        // isn't empty on first launch. Limit to 100 most-recent
        // records (per §3.8.7.4 history-page design).
        if let Some(tx) = cmd_tx.read().clone() {
            // The request id comes from the UI's counter (design doc
            // §3.11.3): the lib's answer names the request.
            let request_id = state_thread.with_mut(|s| s.alloc_request_id());
            let _ = tx.try_send(Request {
                id: request_id,
                command: Command::LoadHistory {
                    limit: Some(100),
                    after: None,
                },
            });
        }
        // Per the /code-review medium perf finding: hoist the
        // .read().clone() out of the event loop. config and
        // cmd_tx are set once at startup (v1: no Save button, no
        // runtime config reload) so caching is safe. Saves one
        // SyncStorage read-lock + Arc clone per lib event.
        let cmd_tx_cached = cmd_tx.read().clone();
        std::thread::Builder::new()
            .name("rencoder-ui-drain".to_string())
            .spawn(move || {
                if let Some(tx) = cmd_tx_cached.as_ref() {
                    while let Ok(evt) = evt_rx.recv() {
                        state_thread.with_mut(|s| reduce(s, Msg::Lib(evt), tx));
                    }
                }
            })
            .expect("rencoder-ui-drain: failed to spawn event-pump thread");

        // Log drain thread (§3.8.7.5 unified log pipeline):
        // tracing_init owns the Sender side; we cloned the Receiver
        // out of LOG_RECEIVER and capture state for `with_mut`.
        // Each line goes through Msg::LibLogLine -> reducer, which
        // pushes to state.log with source = LogSource::LibInternal.
        // Note: Msg::LibLogLine doesn't dispatch to cmd_tx or use
        // config, but the reducer signature requires both for
        // symmetry with other arms. We pass the cached values
        // (no per-line read) — they're cheap borrows.
        if let Some(rx_ref) = tracing_init::LOG_RECEIVER.get() {
            let log_rx = rx_ref.clone();
            let mut state_for_log = state;
            // Re-read the signals for the log drain thread. The
            // cached values above were moved into the event drain
            // thread, so we re-read once (cheap — single read +
            // Arc clone). This is the only re-read; the log drain
            // thread itself caches these values for its loop.
            let cmd_tx_for_log = cmd_tx.read().clone();
            std::thread::Builder::new()
                .name("rencoder-ui-log-drain".to_string())
                .spawn(move || {
                    // LibLogLine never dispatches; Sender is only
                    // here to satisfy the reducer signature.
                    let tx = cmd_tx_for_log
                        .as_ref()
                        .expect("cmd_tx must be set by use_hook before log drain runs");
                    while let Ok(line) = log_rx.recv() {
                        state_for_log.with_mut(|s| reduce(s, Msg::LibLogLine(line), tx));
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
            TopBar { state: state, cmd_tx: cmd_tx }
            if let Some(ref err) = last_error {
                div { class: "global-error", "{err}" }
            }
            div { class: "page",
                match active_page {
                    Page::Setup => rsx! { SetupPage { state: state, cmd_tx: cmd_tx } },
                    Page::Task => rsx! { TaskPage { state: state, cmd_tx: cmd_tx } },
                    Page::History => rsx! { HistoryPage { state: state, cmd_tx: cmd_tx } },
                    Page::Log => rsx! { LogPage { state: state } },
                }
            }
            FooterLog { state: state }
            // Toaster — fixed-position top-right stack of
            // transient user-action feedback toasts (review
            // #133 v1.2). See design.md §3.8.7.2.
            Toaster { state: state }
        }
    }
}

// ============================================================================
// Toaster — fixed top-right stack of transient feedback toasts
// ============================================================================

/// Top-right toast stack (review #133 v1.2). Each toast renders
/// as a fixed-position card with a colored left stripe (driven
/// by `ToastKind`), the message, and a × dismiss button. Auto-
/// dismisses after `duration_ms` via a 250ms tick that prunes
/// expired entries.
///
/// The auto-dismiss loop is implemented with `use_future` +
/// `cx.sleep` so it integrates with Dioxus's scheduler — no
/// raw `std::thread::spawn`. The future re-renders the component
/// on every tick so the stack updates.
#[component]
fn Toaster(state: Signal<AppState, SyncStorage>) -> Element {
    // Snapshot the toasts once per render. The auto-dismiss
    // loop calls state.read() (not state.with_mut()) so the
    // re-render is read-only — the loop's only mutating effect
    // is pruning via state.with_mut() when an entry expires.
    let toasts = state.read().toasts.clone();

    // Auto-dismiss tick — every 250ms, scan for expired
    // toasts and pop them from the front. We don't need a
    // sleep cancellation — the future lives for the lifetime
    // of the App() component, so it terminates when the app
    // closes.
    use_future(move || async move {
        loop {
            // Wait 250ms before each scan. tokio is a transitive
            // dep of dioxus 0.6, so its async runtime is
            // available in use_future blocks.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            // Now scan + prune. Holds a write lock briefly.
            let now = chrono::Utc::now();
            let expired_ids: Vec<u64> = state
                .read()
                .toasts
                .iter()
                .filter(|t| {
                    let elapsed = now.signed_duration_since(t.created_at);
                    elapsed.num_milliseconds() as u64 >= t.duration_ms as u64
                })
                .map(|t| t.id)
                .collect();
            if !expired_ids.is_empty() {
                state.with_mut(|s| {
                    s.toasts.retain(|t| !expired_ids.contains(&t.id));
                });
            }
        }
    });

    rsx! {
        // z-index 20 (above TopBar=10 and task-button-bar=10).
        // Top-right corner (top: 60px clears the sticky TopBar;
        // right: 16px aligns with .app padding).
        div { class: "toaster-container",
            for t in toasts.iter() {
                ToastCard { toast: t.clone(), state: state }
            }
        }
    }
}

/// One toast card. Rendered by `Toaster`. The × button
/// removes the toast from the queue (immediate, no auto-dismiss
/// wait).
#[component]
fn ToastCard(toast: Toast, state: Signal<AppState, SyncStorage>) -> Element {
    let kind_class = match toast.kind {
        ToastKind::Info => "toast-info",
        ToastKind::Warning => "toast-warning",
        ToastKind::Error => "toast-error",
    };
    let icon = match toast.kind {
        ToastKind::Info => "ⓘ",
        ToastKind::Warning => "⚠",
        ToastKind::Error => "⚡",
    };
    let aria_live = match toast.kind {
        ToastKind::Error => "assertive",
        ToastKind::Info | ToastKind::Warning => "polite",
    };
    let id = toast.id;
    rsx! {
        div {
            key: "{id}",
            class: "toast {kind_class}",
            role: "alert",
            aria_live: aria_live,
            span { class: "toast-icon", "{icon}" }
            span { class: "toast-msg", "{toast.message}" }
            button {
                class: "toast-dismiss",
                title: "Dismiss",
                onclick: move |_| {
                    state.with_mut(|s| s.toasts.retain(|t| t.id != id));
                },
                "×"
            }
        }
    }
}

// ============================================================================
// TopBar — 4 tabs + Exit button (visible on all pages)
// ============================================================================

#[component]
fn TopBar(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Request>>, SyncStorage>,
) -> Element {
    let active = state.read().active_page;
    let tx_exit = cmd_tx.read().clone();

    rsx! {
        header { class: "top-bar",
            span { class: "brand", "RENENCODER" }
            nav { class: "tabs",
                tab_button { state: state, cmd_tx: cmd_tx, page: Page::Setup, active: active, label: "SETUP" }
                tab_button { state: state, cmd_tx: cmd_tx, page: Page::Task, active: active, label: "TASK" }
                tab_button { state: state, cmd_tx: cmd_tx, page: Page::History, active: active, label: "HISTORY" }
                tab_button { state: state, cmd_tx: cmd_tx, page: Page::Log, active: active, label: "LOG" }
            }
            div { class: "top-bar-right",
                if let Some(tx) = tx_exit {
                    button {
                        class: "exit-btn",
                        onclick: move |_| {
                            // v1 Exit semantics (per §3.8.7.4):
                            // an active batch -> cancel it, then
                            // window().close() (after BatchCancelled
                            // in v1.1; v1 closes immediately after
                            // the dispatch).
                            // None/Done -> close immediately.
                            //
                            // The cancel goes through the reducer:
                            // it names the batch (design doc
                            // §3.11.3) and carries the request id.
                            state.with_mut(|s| reduce(s, Msg::UserClickedCancel, &tx));
                            desktop::window().close();
                        },
                        "Exit"
                    }
                }
            }
        }
    }
}

/// One TopBar tab button. Extracted from the old 4-copy inline
/// closure pattern so each new page (§3.8.7.2's 4 tabs) doesn't
/// add another copy of the same `onclick` body.
#[component]
fn tab_button(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Request>>, SyncStorage>,
    page: Page,
    active: Page,
    label: &'static str,
) -> Element {
    rsx! {
        button {
            class: if active == page { "tab tab-active" } else { "tab" },
            onclick: move |_| {
                if let Some(tx) = cmd_tx.read().clone() {
                    state.with_mut(|s| reduce(s, Msg::UserClickedTab(page), &tx));
                }
            },
            "{label}"
        }
    }
}

// ============================================================================
// SetupPage — pure config editor (review #133 part 3b).
//
// No [SETUP] header, no DirPicker, no Start button — those moved
// to the Task tab. This page is a form that mirrors the **effective
// config** (the file layer `config_draft` plus the entry layer
// `config_edits`, design doc §3.10.5); every input/radio change
// dispatches `Msg::UserEditedConfigField`, which the reducer
// records in the entry layer + fires `Command::SaveConfig` for
// atomic TOML write. The save status line reads
// `state.config_save_status` ("Saving…" / "Saved at HH:MM:SS").
// ============================================================================

#[component]
fn SetupPage(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Request>>, SyncStorage>,
) -> Element {
    // Snapshot the values needed to render the form. The form
    // is fully controlled — every input reads from `draft` and
    // every change dispatches a Msg.
    // The effective config: the file layer with the user's unsaved
    // edits on top (design doc §3.10.5). The form shows what the
    // run will use.
    let draft = state
        .read()
        .effective_config()
        .encode
        .clone()
        .unwrap_or_default();
    let save_status = state.read().config_save_status.clone();
    let tx_opt = cmd_tx.read().clone();

    rsx! {
        section { class: "setup-page",
            // No [SETUP] header — the page label is the section label.
            // The Setup tab itself is already named "SETUP" in TopBar.

            div { class: "settings-form",
                span { class: "section-label", "SETTINGS" }

                // ---- FORMAT (radio group) ----
                div { class: "form-row",
                    label { class: "form-label", "format" }
                    div { class: "form-radios",
                        for fmt in [FormatArg::Av1, FormatArg::Hevc, FormatArg::Vp9, FormatArg::X264] {
                            {
                                let checked = draft.format == Some(fmt);
                                let kind = ConfigFieldKind::Format;
                                let value = ConfigFieldValue::Format(fmt);
                                // Clone the signals + tx once per closure
                                // so each `move` closure captures its own
                                // copy. Signals are cheap (Arc-like), Sender
                                // is cloneable; the verbosity is worth it
                                // for the ownership clarity.
                                let mut state_c = state;
                                let tx_c = tx_opt.clone();
                                rsx! {
                                    label { class: "radio",
                                        input {
                                            r#type: "radio",
                                            name: "format",
                                            value: "{fmt:?}",
                                            checked: "{checked}",
                                            onchange: move |_| {
                                                        if let Some(tx) = tx_c.clone() {
                                                    state_c.with_mut(|s| {
                                                        reduce(
                                                            s,
                                                            Msg::UserEditedConfigField {
                                                                kind,
                                                                value: value.clone(),
                                                            },
                                                            &tx,
                                                        )
                                                    });
                                                }
                                            },
                                        }
                                        " {fmt:?}"
                                    }
                                }
                            }
                        }
                    }
                }

                // ---- CRF (numeric input) ----
                div { class: "form-row",
                    label { class: "form-label", "crf" }
                    {
                        let tx_for_crf = tx_opt.clone();
                        rsx! {
                            input {
                                class: "form-input",
                                r#type: "number",
                                min: "0",
                                max: "63",
                                step: "1",
                                value: "{draft.crf}",
                                oninput: move |evt| {
                                    let Ok(v) = evt.value().parse::<u32>() else {
                                        return;
                                    };
                                                        if let Some(tx) = tx_for_crf.clone() {
                                        state.with_mut(|s| {
                                            reduce(
                                                s,
                                                Msg::UserEditedConfigField {
                                                    kind: ConfigFieldKind::Crf,
                                                    value: ConfigFieldValue::Crf(v),
                                                },
                                                &tx,
                                            )
                                        });
                                    }
                                },
                            }
                        }
                    }
                    span { class: "form-hint", "0 = bitrate mode (see [encode].bitrate)" }
                }

                // ---- MAX FPS (radio group) ----
                div { class: "form-row",
                    label { class: "form-label", "max fps" }
                    div { class: "form-radios",
                        for cap in [None, Some(24u32), Some(25), Some(30), Some(60)] {
                            {
                                let checked = draft.max_fps == cap;
                                let label = match cap {
                                    None => "off".to_string(),
                                    Some(n) => n.to_string(),
                                };
                                let kind = ConfigFieldKind::MaxFps;
                                let value = ConfigFieldValue::MaxFps(cap);
                                let mut state_c = state;
                                let tx_c = tx_opt.clone();
                                rsx! {
                                    label { class: "radio",
                                        input {
                                            r#type: "radio",
                                            name: "max-fps",
                                            value: "{label}",
                                            checked: "{checked}",
                                            onchange: move |_| {
                                                        if let Some(tx) = tx_c.clone() {
                                                    state_c.with_mut(|s| {
                                                        reduce(
                                                            s,
                                                            Msg::UserEditedConfigField {
                                                                kind,
                                                                value: value.clone(),
                                                            },
                                                            &tx,
                                                        )
                                                    });
                                                }
                                            },
                                        }
                                        " {label}"
                                    }
                                }
                            }
                        }
                    }
                }

                // ---- NVENC (hardware acceleration) ----
                div { class: "form-row",
                    label { class: "form-label", "nvenc" }
                    div { class: "form-radios",
                        for (value, label) in [(true, "on"), (false, "off")] {
                            {
                                let checked = draft.nv.unwrap_or(false) == value;
                                let kind = ConfigFieldKind::Nv;
                                let mut state_c = state;
                                let tx_c = tx_opt.clone();
                                rsx! {
                                    label { class: "radio",
                                        input {
                                            r#type: "radio",
                                            name: "nvenc",
                                            value: "{label}",
                                            checked: "{checked}",
                                            onchange: move |_| {
                                                        if let Some(tx) = tx_c.clone() {
                                                    state_c.with_mut(|s| {
                                                        reduce(
                                                            s,
                                                            Msg::UserEditedConfigField {
                                                                kind,
                                                                value: ConfigFieldValue::Nv(value),
                                                            },
                                                            &tx,
                                                        )
                                                    });
                                                }
                                            },
                                        }
                                        " {label}"
                                    }
                                }
                            }
                        }
                    }
                }

                // ---- NVENC capability (what this machine can run) ----
                {
                    let capability = state.read().nvenc_capability.clone();
                    // Rendered as plain lines: rsx! has no `match` arm
                    // with a block in child position.
                    // The wording comes from the lib so the UI and the
                    // CLI describe the same capability the same way.
                    let lines: Vec<String> = match &capability {
                        None => vec!["not detected yet".to_string()],
                        Some(cap) => cap.lines(),
                    };
                    let review_advisory = capability
                        .as_ref()
                        .and_then(|cap| cap.review_advisory());
                    let mut state_c = state;
                    let tx_c = tx_opt.clone();
                    rsx! {
                        div { class: "form-row",
                            label { class: "form-label", "capability" }
                            div {
                                for line in lines.iter() {
                                    div { class: "plan-row",
                                        span { class: "code", "{line}" }
                                    }
                                }
                                {
                                    if let Some(advisory) = review_advisory {
                                        rsx! {
                                            span { class: "muted", "{advisory}" }
                                        }
                                    } else {
                                        rsx! {}
                                    }
                                }
                                button {
                                    class: "secondary-btn",
                                    onclick: move |_| {
                                        if let Some(tx) = tx_c.clone() {
                                            state_c.with_mut(|s| {
                                                reduce(s, Msg::UserClickedDetectNv, &tx)
                                            });
                                        }
                                    },
                                    "re-detect"
                                }
                            }
                        }
                    }
                }

                // ---- EXTENSIONS (csv text input) ----
                div { class: "form-row",
                    label { class: "form-label", "extensions" }
                    {
                        let tx_for_ext = tx_opt.clone();
                        rsx! {
                            input {
                                class: "form-input",
                                r#type: "text",
                                placeholder: "mp4, mkk, mov (empty = all)",
                                value: draft.extensions.clone().map(|v| v.join(", ")).unwrap_or_default(),
                                oninput: move |evt| {
                                                        if let Some(tx) = tx_for_ext.clone() {
                                        state.with_mut(|s| {
                                            reduce(
                                                s,
                                                Msg::UserEditedConfigField {
                                                    kind: ConfigFieldKind::Extensions,
                                                    value: ConfigFieldValue::Extensions(evt.value()),
                                                },
                                                &tx,
                                            )
                                        });
                                    }
                                },
                            }
                        }
                    }
                    span { class: "form-hint", "comma-separated; empty = all video files" }
                }

                // ---- SAVE STATUS (read-only) ----
                div { class: "form-row",
                    label { class: "form-label", "save status" }
                    span { class: "save-status", "{save_status}" }
                }
            }
        }
    }
}

// ============================================================================
// TaskPage — DirPicker + count summary + file list + 3-state button
// (review #133 part 3c).
//
// Layout (top to bottom):
//   1. Header bar with [TASK] label + TaskPageButton (top-right)
//   2. DirPicker — moved from Setup page (review #133 part 1)
//   3. Count summary line ("N to encode, M to skip") when known
//   4. BatchStatus badge (current phase)
//   5. File list — dry-run records (active) or final records (history)
//   6. ProgressPanel (during Encoding phase)
// ============================================================================

#[component]
fn DirPicker(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Request>>, SyncStorage>,
) -> Element {
    let input_dir = state.read().input_dir.clone();
    let active_phase = state.read().tasks.active.as_ref().map(|t| t.phase);
    // Picker disabled unless no active task (None / Done).
    let enabled = active_phase.is_none() || matches!(active_phase, Some(BatchPhase::Done));

    rsx! {
        // v1.2 (review #133 follow-up): one-row layout — label
        // + path + button on a single line. The label stays
        // visible (so users know what this section is for)
        // but the path truncates with ellipsis if too long.
        // Saves ~50px vertical space vs the previous 3-row
        // layout (label + path + button stacked).
        div { class: "dir-picker",
            label { class: "dir-picker-label", "DIR" }
            div { class: "dir-display",
                if let Some(ref d) = input_dir {
                    span { class: "path", title: "{d.display()}", "{d.display()}" }
                } else {
                    span { class: "muted", "(not set)" }
                }
            }
            button {
                class: "secondary-btn",
                disabled: !enabled,
                onclick: move |_| {
                    // v1.2 (review #133 follow-up): use the
                    // async file dialog + spawn on tokio so the
                    // modal doesn't freeze the Dioxus main
                    // thread. The sync version blocked for the
                    // entire duration the user spent in the
                    // picker (no re-renders, no Exit click).
                    let mut state_async = state;
                    let cmd_tx_async = cmd_tx;
                    tokio::spawn(async move {
                        let result = rfd::AsyncFileDialog::new()
                            .set_title("Choose input directory")
                            .pick_folder()
                            .await;
                        if let Some(handle) = result {
                            let path = handle.path().to_path_buf();
                            if let Some(tx) = cmd_tx_async.read().clone() {
                                state_async.with_mut(|st| {
                                    reduce(st, Msg::UserSelectedDir(path), &tx)
                                });
                            }
                        }
                    });
                },
                "pick directory"
            }
        }
    }
}

/// The badge + counts the task page shows.
///
/// The page pulls these out of the active task (or the latest history
/// entry). This used to be a seven-element positional tuple that every
/// use site had to destructure by hand -- adding one number meant
/// editing the tuple *and* the destructure, and a reader had to count
/// positions to know which field was which.
#[derive(Clone, PartialEq)]
struct BatchSummary {
    task_id: String,
    phase: BatchPhase,
    total_files: usize,
    to_encode: usize,
    to_failed: usize,
    cancelled: bool,
}

#[component]
fn TaskPage(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Request>>, SyncStorage>,
) -> Element {
    let active = state.read().tasks.active.clone();
    let history = state.read().task_history.clone();
    let progress = state.read().progress.clone();
    // The capability report for the current working directory (design
    // doc §3.10.7): the lib checked and wrote the file, the UI reads
    // it — nobody recomputes the check.
    let capability_lines = state
        .read()
        .capability_report
        .as_ref()
        .map(|report| report.lines())
        .unwrap_or_default();

    // Pick the source of records to show in the file list. Active task
    // wins (its dry-run records during Analyzing, its records during
    // Encoding / Done). If no active task, fall back to the most
    // recent history entry.
    let (records_for_list, summary) = if let Some(task) = active.as_ref() {
        // Pick the file list source by phase (v1.2 review
        // #133 follow-up):
        //   Analyzing / AwaitingConfirm: dry-run records
        //     (the user reviews what will be encoded)
        //   Encoding / Done: per-file encode records
        //     (the user watches progress; the file shows
        //      'Succeeded' only after ffmpeg completes it).
        // v1.2 follow-up #2: phase-aware file list source.
        // - Analyzing / AwaitingConfirm: file list area renders
        //   EMPTY — dry-run results surface only via toast
        //   (§3.8.7.6). Rendering dry_run_records here would
        //   re-introduce the "show Succeeded during AwaitingConfirm"
        //   bug (#1 in the user feedback).
        // - Encoding: render records (per-file FileCompleted
        //   events fill this in real time).
        // - Done / Cancelled: render records (final outcomes).
        let list = match task.phase {
            BatchPhase::Analyzing | BatchPhase::AwaitingConfirm | BatchPhase::Starting => {
                Vec::new()
            }
            BatchPhase::Encoding
            | BatchPhase::Cancelling
            | BatchPhase::Done
            | BatchPhase::Cancelled => task.records.clone(),
        };
        (
            list,
            BatchSummary {
                task_id: task.task_id.as_str().to_string(),
                phase: task.phase,
                total_files: task.total_files,
                to_encode: task.to_encode,
                to_failed: task.to_failed,
                cancelled: task.cancelled,
            },
        )
    } else if let Some(latest) = history.first() {
        (
            latest.records.clone(),
            BatchSummary {
                task_id: latest.task_id.as_str().to_string(),
                phase: latest.phase,
                total_files: latest.total_files,
                to_encode: latest.to_encode,
                to_failed: latest.to_failed,
                cancelled: latest.cancelled,
            },
        )
    } else {
        (
            Vec::new(),
            BatchSummary {
                task_id: String::new(),
                phase: BatchPhase::Done,
                total_files: 0,
                to_encode: 0,
                to_failed: 0,
                cancelled: false,
            },
        )
    };

    rsx! {
        section { class: "task-page",
            // v1.2 (review #133 follow-up): drop the
            // `──[ TASK ]────` page header. The TopBar tab
            // already names the page; the header only existed
            // to host the top-right button — which now lives
            // in the sticky bottom row below.
            //
            // Body order: DirPicker -> count summary -> badge
            // -> file list -> progress -> bottom button bar.

            DirPicker { state: state, cmd_tx: cmd_tx }

            // The capability report — one line per check, written by
            // the lib into the operation directory (design doc
            // §3.10.7). It answers "can this directory be worked in".
            if !capability_lines.is_empty() {
                div { class: "capability-report",
                    span { class: "section-label", "CAPABILITY" }
                    for line in capability_lines.iter() {
                        div { class: "capability-line", "{line}" }
                    }
                }
            }

            // Count summary — only when at least one phase has run
            // a dry-run (to_encode/to_failed > 0 means the dry-run is
            // done). When no active task and no history, skip.
            if active.is_some()
                && (summary.to_encode > 0
                    || summary.to_failed > 0
                    || summary.total_files > 0)
            {
                div { class: "count-summary",
                    span { class: "count-encode", "{summary.to_encode} to encode" }
                    span { class: "sep", " · " }
                    span { class: "count-skip", "{summary.to_failed} failed" }
                    span { class: "sep", " · " }
                    span { class: "count-total", "{summary.total_files} total" }
                }
            }

            if active.is_some() || !history.is_empty() {
                BatchStatus { summary: summary.clone() }
            }

            // File list — dry-run records (with `?` for unknown
            // outcomes like Failed), or final records after a run.
            if !records_for_list.is_empty() {
                div { class: "task-plan-panel",
                    span { class: "section-label", "FILES" }
                    div { class: "plan-table-wrap",
                        for r in records_for_list.iter() {
                            div { class: "plan-row",
                                span { class: "path", "{r.source_path}" }
                                span {
                                    class: match r.outcome {
                                        rencoder_lib::types::EncodingOutcome::Ready
                                        | rencoder_lib::types::EncodingOutcome::Succeeded => "outcome-encode",
                                        rencoder_lib::types::EncodingOutcome::Failed => "outcome-fail",
                                        _ => "outcome-skip",
                                    },
                                    "{r.outcome:?}"
                                }
                            }
                        }
                    }
                }
            } else if active.is_none() && history.is_empty() {
                p { class: "muted", "No task running. Pick a directory above, then click the button below." }
            }

            if !progress.is_empty() && active.is_some() {
                ProgressPanel { state: state }
            }

            // Bottom button bar — sticky, shows only when needed.
            // TaskPageButton internally decides hidden vs visible
            // (per §3.8.7.2 state table).
            div { class: "task-button-bar",
                TaskPageButton { state: state, cmd_tx: cmd_tx }
            }
        }
    }
}

/// Contextual button on the Task page — phase-driven state
/// machine. The button lives in the sticky bottom bar and is
/// always visible (v1.2); when there's no action available it
/// renders as a disabled placeholder so the user can tell at a
/// glance what they need to do next.
///
/// States:
///  - **No active task, no history, no dir picked**: disabled
///    "Pick a directory first" — placeholder; user uses the
///    DirPicker at the top of the page to enable Analyze.
///  - **No active task, dir picked, no history**: enabled
///    "Analyze ▸" — Clicking dispatches `Msg::UserClickedStart`
///    (reducer's None arm runs the dry-run flow).
///  - **No active task, has history**: enabled "Re-run ▸" —
///    same Msg as Analyze; reducer's None arm runs the
///    dry-run flow after taking the prior Done task.
///  - **Analyzing**: disabled "Analyze…" — defense-in-depth
///    against Cancel during Analyzing (review #100 stays
///    pending; the lib doesn't honor cancel_token until
///    encoding starts).
///  - **Encoding**: enabled "Cancel ✕" — dispatches
///    `Msg::UserClickedCancel`.
///
/// Active-task Done is unreachable under the auto-encode flow
/// (the reducer moves Done tasks to history immediately), so
/// the `Some(Done)` arm is defensive — renders a disabled
/// placeholder rather than nothing.
#[component]
fn TaskPageButton(
    state: Signal<AppState, SyncStorage>,
    cmd_tx: Signal<Option<Sender<Request>>, SyncStorage>,
) -> Element {
    let active = state.read().tasks.active.clone();
    let history = state.read().task_history.clone();
    let input_dir = state.read().input_dir.clone();
    let phase = active.as_ref().map(|t| t.phase);
    let tx_opt = cmd_tx.read().clone();
    let Some(tx) = tx_opt else {
        return rsx! { span {} };
    };

    let kind = task_button_decision(phase, !history.is_empty(), input_dir.is_some());
    match kind {
        TaskButtonKind::Disabled { label } => rsx! {
            button {
                class: "primary-btn",
                disabled: true,
                "{label}"
            }
        },
        TaskButtonKind::EnabledPrimary { label } => {
            // Encode / Re-run / Analyze: same click handler
            // (Msg::UserClickedStart, the reducer's phase-aware
            // arm dispatches DryRun/StartBatch depending on
            // current state).
            let tx_s = tx.clone();
            rsx! {
                button {
                    class: "primary-btn",
                    onclick: move |_| {
                        state.with_mut(|s| reduce(s, Msg::UserClickedStart, &tx_s));
                    },
                    "{label}"
                }
            }
        }
        TaskButtonKind::EnabledSecondary { label } => {
            // Cancel ✕: only fires during Encoding.
            let tx_c = tx.clone();
            rsx! {
                button {
                    class: "secondary-btn",
                    onclick: move |_| {
                        state.with_mut(|s| reduce(s, Msg::UserClickedCancel, &tx_c));
                    },
                    "{label}"
                }
            }
        }
    }
}

/// Pure decision shape for `TaskPageButton`. Extracted so the
/// 8-path state machine is testable without a Dioxus runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskButtonKind {
    Disabled { label: &'static str },
    EnabledPrimary { label: &'static str },
    EnabledSecondary { label: &'static str },
}

/// Pure state → button decision. Drives `TaskPageButton`'s label +
/// enabled/disabled state based on phase + history + input_dir.
/// No signals, no closures, no IO.
fn task_button_decision(
    phase: Option<BatchPhase>,
    has_history: bool,
    has_input_dir: bool,
) -> TaskButtonKind {
    match phase {
        // Active task in flight: button reflects the phase.
        Some(BatchPhase::Analyzing) => TaskButtonKind::Disabled {
            label: "Analyze…"
        },
        // v1.2 follow-up #2: dry-run finished; user reviews the
        // summary toast, then clicks Encode. Plain "Encode" — no
        // count suffix (we don't design for encoding a subset;
        // §3.8.7.1 decision 2).
        Some(BatchPhase::AwaitingConfirm) => TaskButtonKind::EnabledPrimary { label: "Encode" },
        // v1.2 follow-up #2: post-cancel reuses the
        // AwaitingConfirm arm (same Msg::UserClickedStart reducer
        // path). Same label so the user sees consistent affordance
        // whether they're starting fresh or resuming after cancel.
        Some(BatchPhase::Cancelled) => TaskButtonKind::EnabledPrimary { label: "Encode" },
        Some(BatchPhase::Encoding) => TaskButtonKind::EnabledSecondary {
            label: "Cancel ✕"
        },
        // The batch is on its way: the lib has not answered yet, and
        // the batch id (which a cancel must name -- design doc
        // §3.11.3) is not known. Nothing to press.
        Some(BatchPhase::Starting) => TaskButtonKind::Disabled {
            label: "Starting…"
        },
        // The cancel is on its way: the batch is still finishing.
        Some(BatchPhase::Cancelling) => TaskButtonKind::Disabled {
            label: "Cancelling…",
        },
        // Defensive — finalize moves Done tasks to history
        // immediately, so this branch is rarely hit.
        Some(BatchPhase::Done) => TaskButtonKind::Disabled { label: "Done" },
        // No active task: pick by history then by dir.
        None => {
            if has_history {
                TaskButtonKind::EnabledPrimary {
                    label: "Re-run ▸"
                }
            } else if has_input_dir {
                TaskButtonKind::EnabledPrimary {
                    label: "Analyze ▸"
                }
            } else {
                TaskButtonKind::Disabled {
                    label: "Pick a directory first",
                }
            }
        }
    }
}

#[component]
fn BatchStatus(summary: BatchSummary) -> Element {
    let BatchSummary {
        task_id,
        phase,
        total_files,
        to_encode,
        to_failed,
        cancelled,
    } = summary;
    let badge = match phase {
        BatchPhase::Analyzing => "[● ANALYZING]",
        BatchPhase::AwaitingConfirm => "[● READY]",
        BatchPhase::Starting => "[● STARTING]",
        BatchPhase::Encoding => "[● ENCODING]",
        BatchPhase::Cancelling => "[● CANCELLING]",
        BatchPhase::Done => "[● DONE]",
        BatchPhase::Cancelled => "[● CANCELLED]",
    };
    let badge_class = match phase {
        BatchPhase::Analyzing
        | BatchPhase::AwaitingConfirm
        | BatchPhase::Starting
        | BatchPhase::Encoding
        | BatchPhase::Cancelling => "badge badge-active",
        BatchPhase::Done => "badge badge-done",
        // Cancelled: distinct visual from Done (terminal
        // success) — uses a warning-style class so the user
        // doesn't mistake a cancelled batch for a successful
        // one. Style defined in style.css as `.badge-cancelled`
        // (v1.2 follow-up #2).
        BatchPhase::Cancelled => "badge badge-cancelled",
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
                    span { class: "code", "→ {to_failed} failed" }
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
    cmd_tx: Signal<Option<Sender<Request>>, SyncStorage>,
) -> Element {
    let history = state.read().history.clone();
    let tx_opt = cmd_tx.read().clone();

    rsx! {
        section { class: "history-page",
            // v1.2 (review #133 follow-up): drop the
            // `──[ HISTORY ]────` page header. Refresh button
            // moves to the top of the body, inline with the
            // content (no header row needed).
            if let Some(tx) = tx_opt {
                div { class: "page-toolbar",
                    button {
                        class: "secondary-btn",
                        onclick: move |_| {
                                        state.with_mut(|s| reduce(s, Msg::UserClickedLoadHistory, &tx));
                        },
                        "Refresh ⟳"
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
            // v1.2 (review #133 follow-up): drop the
            // `──[ LOG ]────` page header. The TopBar tab
            // already names the page; no contextual button on
            // this page.
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
    let rendered: Vec<(String, String, String)> =
        lines.iter().rev().map(format_log_entry).collect();
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
    let rendered: Vec<(String, String, String)> =
        lines.iter().rev().map(format_log_entry).collect();
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

/// Format a `LogLine` for display in `LogPanel` and `FooterLog`.
/// Returns `(time, file_label, message)`:
/// - `time`: HH:MM:SS format of when the line landed in `state.log`
/// - `file_label`: the file index (per-file ffmpeg stderr) or
///   `"global"` (lib-internal tracing events with `NO_FILE_INDEX`)
/// - `message`: the raw line text (already formatted by
///   `tracing-subscriber` for lib events; raw ffmpeg text for
///   `Event::LogLine`).
fn format_log_entry(l: &crate::msg::LogLine) -> (String, String, String) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::BatchPhase::*;

    /// Helper to keep the per-case assertions short. Asserts the
    /// decision is `kind`, ignoring label/variant differences that
    /// are obvious from context.
    fn assert_kind(got: TaskButtonKind, expected: TaskButtonKind) {
        // PartialEq derived on the enum compares all variants;
        // we use that here rather than a hand-written matcher so
        // adding a new variant (Disabled { label } => "x") doesn't
        // silently make a "looks the same" test pass.
        assert_eq!(got, expected);
    }

    #[test]
    fn task_button_analyzing_disabled() {
        // Cancel during Analyzing is review #100 (pending);
        // lib doesn't honor cancel_token in this phase, so we
        // disable the button. Locked by §3.8.7.1 limitations.
        assert_kind(
            task_button_decision(Some(Analyzing), false, false),
            TaskButtonKind::Disabled {
                label: "Analyze…"
            },
        );
    }

    #[test]
    fn task_button_awaiting_confirm_encodes() {
        // v1.2 follow-up #2: after dry-run completes, the button
        // label is plain "Encode" (no count suffix — we don't
        // design for encoding a subset; §3.8.7.1 decision 2).
        assert_kind(
            task_button_decision(Some(AwaitingConfirm), false, false),
            TaskButtonKind::EnabledPrimary { label: "Encode" },
        );
    }

    #[test]
    fn task_button_cancelled_also_encodes() {
        // v1.2 follow-up #2: post-cancel arm uses the same
        // Msg::UserClickedStart reducer path as AwaitingConfirm,
        // so the label and click handler are identical. This
        // avoids a special-case in the reducer for Cancelled.
        assert_kind(
            task_button_decision(Some(Cancelled), false, false),
            TaskButtonKind::EnabledPrimary { label: "Encode" },
        );
    }

    #[test]
    fn task_button_encoding_cancels() {
        // Encoding is the only phase where Cancel ✕ is wired to
        // the lib's cancel_token (which the task manager polls
        // between ffmpeg frames).
        assert_kind(
            task_button_decision(Some(Encoding), false, false),
            TaskButtonKind::EnabledSecondary {
                label: "Cancel ✕"
            },
        );
    }

    #[test]
    fn task_button_done_disabled_defensive() {
        // Done is unreachable in practice (finalize moves Done
        // tasks to history immediately), but the arm is defensive
        // — show a disabled placeholder rather than nothing so the
        // bottom bar is never empty.
        assert_kind(
            task_button_decision(Some(Done), false, false),
            TaskButtonKind::Disabled { label: "Done" },
        );
    }

    #[test]
    fn task_button_none_with_history_reruns() {
        // History present → user can re-run the dry-run flow.
        // history length > 0 wins over has_input_dir.
        assert_kind(
            task_button_decision(None, true, true),
            TaskButtonKind::EnabledPrimary {
                label: "Re-run ▸"
            },
        );
        assert_kind(
            task_button_decision(None, true, false),
            TaskButtonKind::EnabledPrimary {
                label: "Re-run ▸"
            },
        );
    }

    #[test]
    fn task_button_none_no_history_with_dir_analyzes() {
        // No history + dir picked = "Analyze ▸" — fresh first run.
        assert_kind(
            task_button_decision(None, false, true),
            TaskButtonKind::EnabledPrimary {
                label: "Analyze ▸"
            },
        );
    }

    #[test]
    fn task_button_none_nothing_disabled_pick_first() {
        // No history + no dir = "Pick a directory first" — disabled
        // placeholder that tells the user what to do next (rather
        // than an empty button).
        assert_kind(
            task_button_decision(None, false, false),
            TaskButtonKind::Disabled {
                label: "Pick a directory first",
            },
        );
    }

    #[test]
    fn task_button_phase_wins_over_history_and_dir() {
        // Defense in depth: even if a race sets history/dir while
        // an active task is in flight, the phase arm must take
        // precedence. Otherwise the bottom button would suddenly
        // flip from "Cancel ✕" to "Re-run ▸" mid-encode.
        assert_kind(
            task_button_decision(Some(Encoding), true, true),
            TaskButtonKind::EnabledSecondary {
                label: "Cancel ✕"
            },
        );
    }

    #[test]
    fn format_log_entry_uses_global_label_for_lib_events() {
        // Per-file ffmpeg stderr uses the file_index; lib-internal
        // tracing events use NO_FILE_INDEX sentinel which renders
        // as "global" in the log panel.
        use crate::msg::{LogLine, LogSource, NO_FILE_INDEX};
        use chrono::{TimeZone, Utc};
        let lib_line = LogLine {
            file_index: NO_FILE_INDEX,
            source: LogSource::LibInternal,
            line: "lib event".to_string(),
            at: Utc.with_ymd_and_hms(2026, 1, 1, 12, 34, 56).unwrap(),
        };
        let (time, file_label, message) = format_log_entry(&lib_line);
        assert_eq!(time, "12:34:56");
        assert_eq!(file_label, "global");
        assert_eq!(message, "lib event");
    }

    #[test]
    fn format_log_entry_uses_index_label_for_ffmpeg_stderr() {
        // Per-file ffmpeg stderr carries a real file_index (the
        // batch's per-file index, not the global sentinel).
        use crate::msg::{LogLine, LogSource};
        use chrono::{TimeZone, Utc};
        let ffmpeg_line = LogLine {
            file_index: 2,
            source: LogSource::FfmpegStderr,
            line: "frame= 100".to_string(),
            at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        };
        let (_, file_label, _) = format_log_entry(&ffmpeg_line);
        assert_eq!(file_label, "2");
    }
}
