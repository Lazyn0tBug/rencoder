//! Tracing initialization for the UI binary (§3.8.7.5).
//!
//! v1.2 follow-up #4: lib emits `tracing` events directly (was
//! `log` bridged via `tracing-log`). The bridge is dropped here;
//! UI consumes lib events the same way it consumes its own
//! tracing events — single channel, single subscriber.
//!
//! Sink: UI doesn't have a console (desktop app), so events
//! flow into a `crossbeam_channel::Sender<String>` drained by
//! a dedicated thread that pushes into `state.log` via the reducer.
//!
//! Flow:
//!
//! ```text
//! lib `tracing::info!` -> tracing subscriber (this module)
//!     -> custom MakeWriter -> crossbeam_channel::Sender
//!     -> log drain thread (App's use_hook) -> Msg::LibLogLine
//!     -> reducer -> state.log.push_back -> Dioxus re-render
//! ```
//!
//! The receiver side of the channel is parked in `LOG_RECEIVER`
//! (a `OnceLock`) — `init()` writes it; App's `use_hook` takes it
//! and spawns the drain thread. We use a `OnceLock` rather than
//! plumbing the receiver through main.rs → App() because the
//! Dioxus launch path doesn't take constructor args.

use std::io;
use std::sync::OnceLock;

use crossbeam_channel::{unbounded, Receiver, Sender};
use tracing_subscriber::fmt::MakeWriter;

/// The receiver side of the log channel, populated by `init()` and
/// taken (cloned) by App's `use_hook` for the drain thread.
///
/// `OnceLock` rather than `Mutex<Option<...>>`: init() runs once
/// before `dioxus::launch` (per main.rs), App's `use_hook` runs
/// once per App() mount. There's no contention scenario where
/// `set` happens after `get`.
pub static LOG_RECEIVER: OnceLock<Receiver<String>> = OnceLock::new();

/// Initialize the global tracing subscriber.
///
/// Idempotent: subsequent calls are no-ops (subscriber init
/// returns `Err` and is swallowed). The receiver becomes
/// available via `LOG_RECEIVER.get()`.
pub fn init() {
    let (tx, rx) = unbounded::<String>();
    LOG_RECEIVER
        .set(rx)
        .expect("tracing_init::init called twice");

    let writer = ChannelWriter { tx };

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .with_writer(writer)
        .with_env_filter(filter)
        .try_init();
}

/// `MakeWriter` impl that pushes formatted log lines into a
/// `crossbeam_channel::Sender<String>`.
///
/// Per-event call: `make_writer()` returns a `ChannelWriterHandle`
/// (the inner Writer). The tracing fmt layer calls `write()` zero
/// or more times then `flush()` once per event. We accumulate
/// bytes in the handle and send on flush (one logical line per
/// event).
struct ChannelWriter {
    tx: Sender<String>,
}

impl<'a> MakeWriter<'a> for ChannelWriter {
    type Writer = ChannelWriterHandle;
    fn make_writer(&'a self) -> Self::Writer {
        ChannelWriterHandle {
            buf: String::new(),
            tx: self.tx.clone(),
        }
    }
}

struct ChannelWriterHandle {
    buf: String,
    tx: Sender<String>,
}

impl io::Write for ChannelWriterHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // tracing-subscriber's fmt layer may call write() multiple
        // times per event (e.g. timestamp + level + message + \n).
        // Accumulate into `buf`; flush() sends one logical line.
        self.buf.push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Strip trailing newline (the fmt layer always adds one);
        // send as a single line. If the receiver has been dropped
        // (drain thread exited), `send` returns Err — silently
        // swallow. Logging must not panic the UI.
        let line = std::mem::take(&mut self.buf);
        let line = line.trim_end_matches('\n').to_string();
        if !line.is_empty() {
            let _ = self.tx.send(line);
        }
        Ok(())
    }
}
