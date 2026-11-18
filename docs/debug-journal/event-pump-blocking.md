# Debug Journal: `use_coroutine` + Blocking `crossbeam::recv()` Deadlocks Dioxus 0.6 Executor

> **TL;DR** — Dioxus 0.6 `use_coroutine` + `async move { while let Ok(evt) = recv() { ... } }`
> with a *blocking* `Receiver::recv()` freezes the runtime's task poller. Symptom:
> empty "Dioxus App" window, no h1, no error in stderr. Symptom's persistent
> through several "fixes" that look right but aren't. Only bisecting the
> `App()` body line-by-line against a minimal test (the
> `ui-minimal-test` branch) nails it.

## Symptom

Run `rencoder-ui` (the dist binary). Window opens with default "Dioxus App"
title. Content area is **empty** — no `h1`, no panels, no buttons. Closing the
window from the title bar's close button doesn't work; only `Ctrl-C` from the
launching shell kills the process.

## Root cause

`use_coroutine` spawns a task into Dioxus's runtime, which is polled
cooperatively in `VirtualDom::process_events` (dioxus-core, virtual_dom.rs).
Inside that task, an `async move { while let Ok(_) = rx.recv() { ... } }`
calls `crossbeam_channel::Receiver::recv()`. That call is **blocking** — it
does *not* yield. The first iteration of the loop blocks the task. The
runtime's task poller can never make progress past the blocked task. The
App's first render never runs.

Concretely, in Dioxus 0.6:
- `use_coroutine(F)` spawns a task running `F(_rx: UnboundedReceiver<()>)`
- The task is polled inside `VirtualDom::poll_tasks()`
- `poll_tasks` is itself driven by `process_events` (the runtime loop)
- A blocking call in the task blocks *that* task from yielding
- The runtime still ticks the rest of the world, but the App's first
  `process_events` can't complete the render because the coroutine task
  hasn't yielded to release the future it's awaiting

## Bisect log (14 steps)

> The 14 steps below isolate the bug. They ran on the `docs/gui-frontend-design`
> branch in this order. Each step is one bisect commit. Steps marked OK
> produced a visible h1; BROKEN means the empty-frame symptom returned.
> The minimal test (`ui-minimal-test` branch, commit `d0c941d`) is the
> control: just `dioxus::launch(App)` with a 5-line App and a single colored
> div renders the blue screen correctly.

| # | Branch state | What was added | Result |
|---|---|---|---|
| 1 | full App | use_coroutine + DirPicker (no inline style yet) | BROKEN |
| 2 | full App | inline `<style dangerous_inner_html="{css}">` | BROKEN |
| 3 | ui-minimal-test | minimal: no state, no coroutine, colored div | **OK — control** |
| 4 | docs/gui-frontend-design | `<h1>` + 2 inline-styled `<p>` only (no state) | **OK — proves inline style is fine** |
| 5 | → + `use_signal(AppState::default())` | state only | **OK** |
| 6 | → + `use_signal × 3` + `use_coroutine(no-op body)` | signals + coroutine, no body | **OK** |
| 7 | → + `let _s = state.read();` (no-op read) | adds the pre-rsx state.read the original had | **OK — but irrelevant** |
| 8 | → + `use_coroutine(start_controller)` | adds start_controller in the coroutine | **OK** |
| 9 | → + `cmd_tx.set`, `config.set`, `into_split` | adds the signal.set + split calls | **OK** |
| 10 | → + `async move { while let Ok(_) = rx.recv() {} }` | adds the async block with blocking recv | **BROKEN — first reproduce** |
| 11 | → + empty body inside the loop | drop state.with_mut, leave the recv | **BROKEN — pinpoints to recv itself** |
| 12 | → + `use_signal` instead of `use_signal_sync` for the thread (didn't compile, but confirmed Send/Sync requirement) | validates that the *fix* is to use a sync signal | (compile error) |
| 13 | → + removed `let _s = state.read();` | tests if the held ref was the cause | **BROKEN** |
| 14 | → + use_signal_sync + std::thread + direct with_mut from thread | the **fix** | **OK** |

Steps 1–2 reproduced the empty-frame. Step 3 (the minimal control) proved
the launch path itself is fine. Steps 4–9 added the original App's state
plumbing one piece at a time and stayed OK. Step 10 added the
`async move { ... recv() ... }` and the symptom came back. Step 11
confirmed the *recv itself* (not the loop body) is the trigger. Steps 12–13
ruled out other suspects. Step 14 was the fix.

## What didn't work (and why)

### Approach 1: `use_signal_sync` + `use_coroutine` + empty async block
Added `use_signal_sync` for the three cross-thread signals. Switched the
async block to a no-op `async move {}`. **OK** — the coroutine itself is
fine; the `recv()` is the problem.

### Approach 2: `use_signal` (default) + `use_coroutine` + `async move { ... rx.recv() ... }`
Compile error E0277: `RefCell<StorageEntry<RefCellStorageEntryData>> cannot
be shared between threads safely`. **Compile error** — confirms default
`use_signal` (`Signal<T, UnsyncStorage>`) is `!Send`.

### Approach 3: `use_signal` + `use_coroutine` + bridge via `futures::channel::mpsc`
Spawn a `std::thread` to drain `crossbeam::evt_rx` into a `futures::mpsc`,
have `use_coroutine` `await` the futures channel. **OK functionally** but
**architectural smell**:
- extra OS thread for the bridge
- extra in-memory queue
- introduces a direct `futures` crate dependency
- the bridge thread is doing nothing the lib could be doing itself

The user's review (#Tiers follow-up) flagged this as a "compatibility
fix, not the cleanest end state." The right answer is to either:
- **Keep crossbeam at the lib boundary** (chosen — see docs/design.md §3.9)
- **Swap to `async-channel` at the lib boundary** (rejected — see §3.9
  "Alternatives considered")

## What worked (the fix)

A `std::thread::spawn` inside `use_hook` that:
1. Captures `Send + Sync` `SyncSignal` copies of `state`, `config`, `cmd_tx`.
2. Loops on `rx.recv()` (blocking, but on a *dedicated* OS thread).
3. Calls `state.with_mut(|s| reduce(s, Msg::Lib(evt), &tx, &cfg))` —
   `Signal<T, SyncStorage>::with_mut` works across threads because the
   underlying storage is `parking_lot::RwLock<...>` (Send + Sync).
4. Dioxus 0.6's `SignalSubscriberDrop` schedules a re-render via the
   runtime's `SchedulerMsg::Immediate` channel — the OS thread doesn't
   need to be in the Dioxus runtime to trigger re-renders.

This is what `dioxus::hooks` itself uses in its `use_signal_sync` doctest
(use_signal.rs:50-73): spawn a `tokio::spawn` that writes to the signal
from there. The pattern is officially supported.

## Files that matter

- `rencoder_ui/src/app.rs` — the fix lives here
- `rencoder_lib/src/job.rs` — `start_controller()` is unchanged;
  `crossbeam::Sender<Command>` / `Receiver<Event>` are the lib's choice
- `docs/design.md §3.9` — final architecture + alternatives
- `docs/design.md §3.8.7` — `AppState` doc; the panic-gap is a v1
  limitation here (deferred to v2)

## Recovery (out of scope for v1)

If the OS drain thread panics (e.g. a `reduce` bug fires inside
`with_mut`), all future events are silently lost — the lib keeps
writing to `crossbeam::evt_rx` but nobody drains it. v1 doesn't guard
against this; v2 should wrap the loop in `std::panic::catch_unwind` and
surface a "drain thread dead" state via `controller_alive = false`. See
docs/design.md §3.8.7 v1 limitations.

## What I'd do differently next time

The 14-step bisect took ~2 hours because the *minimal test was added too
late*. The control (no state, no coroutine — just a colored div) would
have isolated the launch path in 30 minutes. **Always write the minimal
test FIRST** when debugging "the rendered output is wrong." Bisect from
a known-working control.
