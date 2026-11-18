//! Platform-specific interrupt registration with shared partial-output cleanup.
//! macOS/Linux (Unix) handle SIGINT/SIGTERM/SIGHUP; Windows handles console events.

use parking_lot::Mutex;
use std::sync::Arc;

pub type CurrentOutput = rencoder_lib::pipeline::partial_output::PartialOutputHandle;

pub fn install_handler() -> CurrentOutput {
    let current: CurrentOutput = Arc::new(Mutex::new(None));
    register_handler(current.clone());
    current
}

fn cleanup_and_exit(current: &CurrentOutput, signal: &str) -> ! {
    if let Some(ctx) = current.lock().take() {
        tracing::warn!("    task {}/{} interrupt", ctx.index, ctx.total);
        if let Some(output) = ctx.output.as_ref() {
            if let Err(error) = std::fs::remove_file(output) {
                tracing::warn!("failed to remove {}: {error}", output.display());
            }
        }
        tracing::warn!(
            "{} interrupt at {}, {} files successed",
            ctx.batch_label,
            ctx.batch_path.display(),
            ctx.index.saturating_sub(1)
        );
    } else {
        tracing::warn!("received {signal} (no active batch)");
    }
    std::process::exit(1);
}

fn register_handler(current: CurrentOutput) {
    #[cfg(unix)]
    {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
        use signal_hook::iterator::Signals;
        let mut signals =
            Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("failed to register signal handlers");
        std::thread::spawn(move || {
            if let Some(signal) = signals.forever().next() {
                let name = match signal {
                    SIGINT => "SIGINT",
                    SIGTERM => "SIGTERM",
                    SIGHUP => "SIGHUP",
                    _ => "unknown",
                };
                cleanup_and_exit(&current, name);
            }
        });
    }
    #[cfg(windows)]
    {
        windows::register(current);
    }
}

#[cfg(windows)]
mod windows {
    use super::{cleanup_and_exit, CurrentOutput};
    use std::sync::OnceLock;

    static CURRENT: OnceLock<CurrentOutput> = OnceLock::new();

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    };

    unsafe extern "system" fn on_control(event: u32) -> BOOL {
        let name = match event {
            CTRL_C_EVENT => "CTRL_C_EVENT",
            CTRL_BREAK_EVENT => "CTRL_BREAK_EVENT",
            CTRL_CLOSE_EVENT => "CTRL_CLOSE_EVENT",
            CTRL_LOGOFF_EVENT => "CTRL_LOGOFF_EVENT",
            CTRL_SHUTDOWN_EVENT => "CTRL_SHUTDOWN_EVENT",
            _ => return FALSE,
        };
        if let Some(current) = CURRENT.get() {
            cleanup_and_exit(current, name);
        }
        FALSE
    }

    pub(super) fn register(current: CurrentOutput) {
        assert!(
            CURRENT.set(current).is_ok(),
            "signal handler already installed"
        );
        // SAFETY: static callback uses the Windows ABI and remains valid for
        // the lifetime of the process; shared state is initialized above.
        let registered = unsafe { SetConsoleCtrlHandler(Some(on_control), TRUE) };
        assert_ne!(
            registered,
            FALSE,
            "failed to register console handler: {}",
            std::io::Error::last_os_error()
        );
    }
}
