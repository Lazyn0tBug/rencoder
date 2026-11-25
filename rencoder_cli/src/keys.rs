//! Reading the **quit key** while a batch runs.
//!
//! The keyboard's interrupt (Ctrl-C) says "I was interrupted": the
//! process is killed, the exit code is 130, the shell treats it as a
//! failure. A deliberate quit is a different thing -- the user asks
//! the app to *wrap up* -- and it leaves with the same stop path
//! (cancel request → stop at the file boundary → partial removed →
//! records written) but a different exit code (0). That is the whole
//! difference between the two, and it is the reason for a second key
//! (design doc §3.11.2).
//!
//! The key is **Ctrl-D on both platforms**.
//!
//! - On Unix, Ctrl-D in *canonical* mode is the line discipline's EOF
//!   and never reaches the application, so the reader puts the
//!   terminal into **non-canonical** mode. `ISIG` stays on (Ctrl-C is
//!   still a signal -- the interrupt contract is untouched) and `IXON`
//!   stays on (Ctrl-S / Ctrl-Q flow control is not ours to change).
//! - On Windows, Ctrl-D is not a console control event at all, so the
//!   reader reads **key events** with `ReadConsoleInput` -- the
//!   console mode is not touched.
//!
//! The reader is installed **around the batch only**: the interactive
//! prompt (the NVENC refresh question) reads whole lines from the
//! same stdin, and two readers on one stdin eat each other's
//! characters.
//!
//! The reader thread **stops when the batch is over** and is joined
//! before the terminal is handed back: a reader that lingers would
//! swallow the keystrokes the user types at their shell.

use std::io;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::signal::{self, CancelSlot};

/// What a key press means to us.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Quit gracefully -- Ctrl-D.
    Quit,
    /// Not ours: ignore it.
    Ignore,
}

/// A key press, in a shape both platforms can produce.
///
/// Unix hands us a **byte** (the terminal translates the key), Windows
/// hands us a **key event** (a virtual key code plus the modifier
/// keys that were held). One decision function reads both.
#[derive(Clone, Copy, Debug)]
pub struct KeyPress {
    /// The byte the terminal produced (Unix).
    pub byte: Option<u8>,
    /// The virtual key code (Windows).
    pub virtual_key: Option<u16>,
    /// Whether a control modifier was held.
    pub ctrl: bool,
}

impl KeyPress {
    /// A press the terminal turned into a byte (the Unix path).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn byte(byte: u8) -> Self {
        Self {
            byte: Some(byte),
            virtual_key: None,
            ctrl: false,
        }
    }

    /// A console key event (the Windows path).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn key(virtual_key: u16, ctrl: bool) -> Self {
        Self {
            byte: None,
            virtual_key: Some(virtual_key),
            ctrl,
        }
    }
}

/// The byte Ctrl-D produces (EOT) -- the Unix path.
const CTRL_D: u8 = 0x04;

/// The virtual key code of `D` -- the Windows path.
const VK_D: u16 = 0x44;

/// What a key press means.
///
/// Ctrl-D is the only key we take; everything else is ignored (and
/// dropped -- see the module doc on the terminal mode).
pub fn action_of(press: KeyPress) -> Action {
    let is_ctrl_d = press.byte == Some(CTRL_D) || (press.ctrl && press.virtual_key == Some(VK_D));
    if is_ctrl_d {
        Action::Quit
    } else {
        Action::Ignore
    }
}

/// Watches the keyboard for the quit key while a batch runs.
///
/// The guard stops the reader and puts the terminal back when it is
/// dropped -- see [`restore_terminal`] for the paths that skip the
/// destructors (`process::exit`, a panic).
pub struct KeyReader {
    stop: Arc<AtomicBool>,
    /// Set by the reader when it is done -- so we do not poke the
    /// console for a reader that already left.
    ///
    /// **Read on Windows only**: the console nudge is the only thing
    /// that needs to know the reader is gone. On Unix the reader is
    /// stopped by the flag and joined.
    #[cfg_attr(not(windows), allow(dead_code))]
    finished: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Windows: the console handle, so the reader can be woken up.
    #[cfg(windows)]
    console: Option<windows::Console>,
}

impl KeyReader {
    /// Start watching the keyboard.
    ///
    /// Returns `None` when there is no keyboard to watch -- stdin is
    /// not a terminal (a pipe, a file) or the terminal mode cannot be
    /// changed. The run then behaves exactly as before: Ctrl-C is the
    /// only way to stop it.
    pub fn install(slot: &CancelSlot) -> Option<Self> {
        if !io::stdin().is_terminal() {
            // No keyboard: the run behaves as if the key did not
            // exist. Say so -- "Ctrl-D does nothing" is otherwise a
            // mystery.
            tracing::debug!(
                "stdin is not a terminal: there is no keyboard to watch, \
                 the quit key (Ctrl-D) does nothing"
            );
            return None;
        }
        #[cfg(unix)]
        if let Err(e) = unix::enter() {
            tracing::warn!(
                error = %e,
                "could not put the terminal in non-canonical mode: \
                 the quit key (Ctrl-D) does nothing"
            );
            return None;
        }
        tracing::debug!("watching the keyboard for the quit key (Ctrl-D)");

        #[cfg(windows)]
        let console = windows::Console::open();

        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_finished = Arc::clone(&finished);
        let slot = Arc::clone(slot);
        let handle = std::thread::spawn(move || {
            read_loop(&slot, &thread_stop);
            thread_finished.store(true, Ordering::Relaxed);
        });

        Some(Self {
            stop,
            finished,
            handle: Some(handle),
            #[cfg(windows)]
            console,
        })
    }

    /// Stop the reader and wait for it.
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Windows: the reader is blocked in `ReadConsoleInput`, so it
        // needs a nudge to notice the flag. A reader that already
        // left is not poked -- the nudge would land in the console's
        // input buffer and the user's shell would see it.
        #[cfg(windows)]
        if !self.finished.load(Ordering::Relaxed) {
            if let Some(console) = &self.console {
                console.wake();
            }
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for KeyReader {
    fn drop(&mut self) {
        self.stop_and_join();
        restore_terminal();
    }
}

/// Start watching the keyboard for the quit key.
///
/// See [`KeyReader::install`]: `None` means there is no keyboard to
/// watch, and the run behaves as if the key did not exist.
pub fn install(slot: &CancelSlot) -> Option<KeyReader> {
    KeyReader::install(slot)
}

/// Put the terminal back the way we found it.
///
/// Idempotent, and safe to call from anywhere: the process may leave
/// through an error path, a panic or `process::exit` -- and a terminal
/// left in non-canonical mode takes the user's shell with it (no line
/// editing, no echo).
pub fn restore_terminal() {
    #[cfg(unix)]
    unix::restore();
}

/// Read keys until the quit key arrives (or the reader is stopped).
#[cfg(unix)]
fn read_loop(slot: &CancelSlot, stop: &AtomicBool) {
    // The trait that gives `Stdin` its `read` -- imported here, where
    // it is used: the platform-specific body keeps its own imports
    // (design doc §1.3 -- `cfg` lives in the function that does the
    // platform-specific work, not at the file's top).
    use std::io::Read;

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Wait for a key, but wake up regularly: the reader has to
        // notice that the batch is over, and it must not sit on the
        // user's keystrokes after that.
        if !unix::wait_for_input(100) {
            // Interrupted by a signal: try again.
            continue;
        }
        let mut byte = [0u8; 1];
        match io::stdin().read(&mut byte) {
            // EOF: the input is gone, there is nothing left to read.
            Ok(0) => return,
            Ok(_) => {
                if action_of(KeyPress::byte(byte[0])) == Action::Quit {
                    signal::request_quit(slot);
                    return;
                }
            }
            // A signal interrupted the read: try again.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Read console key events until the quit key arrives.
#[cfg(windows)]
fn read_loop(slot: &CancelSlot, stop: &AtomicBool) {
    use windows_sys::Win32::System::Console::{
        ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, LEFT_CTRL_PRESSED, RIGHT_CTRL_PRESSED,
    };

    let Some(console) = windows::Console::open() else {
        return;
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut input: INPUT_RECORD = unsafe { std::mem::zeroed() };
        let mut events: u32 = 0;
        // SAFETY: the handle is the console's input handle (checked
        // above), and the buffer is a valid INPUT_RECORD.
        let ok = unsafe { ReadConsoleInputW(console.handle, &mut input, 1, &mut events) };
        if ok == 0 {
            // Not a console (a pipe), or the console went away.
            return;
        }
        if events == 0 || u32::from(input.EventType) != KEY_EVENT {
            continue;
        }
        // SAFETY: the event type is KEY_EVENT (checked above), so the
        // union holds a KEY_EVENT_RECORD.
        let key = unsafe { input.Event.KeyEvent };
        if key.bKeyDown == 0 {
            continue;
        }
        let ctrl = key.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
        if action_of(KeyPress::key(key.wVirtualKeyCode, ctrl)) == Action::Quit {
            signal::request_quit(slot);
            return;
        }
    }
}

/// Any other platform: no keyboard to watch.
///
/// The CLI ships for Unix and Windows (design doc §2); this keeps the
/// crate compiling anywhere else -- a run there simply has no quit
/// key.
#[cfg(not(any(unix, windows)))]
fn read_loop(_slot: &CancelSlot, _stop: &AtomicBool) {}

/// The Unix terminal mode and the wait for a key.
#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::AsRawFd;

    use parking_lot::Mutex;

    /// The terminal state we found, so we can put it back.
    ///
    /// A **global**, not a field of the reader: the terminal has to be
    /// restored on paths that skip the destructors (`process::exit`, a
    /// panic), and those paths have no handle on the reader.
    static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

    /// Put the terminal into non-canonical mode.
    ///
    /// - `ICANON` off: a key press reaches us as a byte, not as a
    ///   line. In canonical mode Ctrl-D is the line discipline's EOF
    ///   and never reaches the application.
    /// - `ISIG` **stays on**: Ctrl-C is still a signal -- the
    ///   interrupt contract is untouched.
    /// - `IXON` **stays on**: Ctrl-S / Ctrl-Q flow control is not ours
    ///   to change.
    /// - `ECHO` off: the batch's output is the user's feedback; a
    ///   stray keystroke must not scribble into it.
    pub fn enter() -> io::Result<()> {
        let fd = io::stdin().as_raw_fd();
        // SAFETY: tcgetattr fills the termios we hand it.
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a termios we built from the current one.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        *SAVED.lock() = Some(original);
        Ok(())
    }

    /// Put the terminal back. Idempotent: the second call does
    /// nothing.
    pub fn restore() {
        let Some(original) = SAVED.lock().take() else {
            return;
        };
        let fd = io::stdin().as_raw_fd();
        // SAFETY: `original` is the termios we saved on the way in.
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &original);
        }
    }

    /// Is there a key to read? Waits at most `timeout`.
    ///
    /// `poll` rather than a blocking read, so the reader notices
    /// that the batch is over instead of sitting on the user's
    /// keystrokes.
    pub fn wait_for_input(timeout_ms: i32) -> bool {
        let mut fds = libc::pollfd {
            fd: io::stdin().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `fds` is a valid pollfd, and we pass its address
        // with the length 1 -- the array of one that `poll` expects.
        let ready = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
        if ready < 0 {
            // A signal (EINTR) interrupted the wait: the caller tries
            // again.
            return false;
        }
        ready > 0 && fds.revents & libc::POLLIN != 0
    }
}

/// The Windows console input.
#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, WriteConsoleInputW, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT,
        KEY_EVENT_RECORD, STD_INPUT_HANDLE,
    };

    /// The console's input handle.
    pub struct Console {
        pub handle: HANDLE,
    }

    impl Console {
        /// The console behind stdin, if there is one.
        pub fn open() -> Option<Self> {
            // SAFETY: GetStdHandle with a standard handle id is always
            // safe.
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return None;
            }
            Some(Self { handle })
        }

        /// Nudge the reader: it is blocked in `ReadConsoleInput`, so
        /// it needs an event to notice that it should stop.
        ///
        /// The event is a key press with no key code -- it carries no
        /// meaning, and it is consumed by our own reader.
        pub fn wake(&self) {
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            record.EventType = KEY_EVENT as u16;
            record.Event = INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: 1,
                    ..unsafe { std::mem::zeroed() }
                },
            };
            let mut written: u32 = 0;
            // SAFETY: the handle is the console's input handle, and
            // the buffer holds one INPUT_RECORD.
            unsafe {
                WriteConsoleInputW(self.handle, &record, 1, &mut written);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_d_is_the_quit_key() {
        assert_eq!(action_of(KeyPress::byte(0x04)), Action::Quit);
        assert_eq!(action_of(KeyPress::byte(b'q')), Action::Ignore);
        assert_eq!(action_of(KeyPress::byte(b'\r')), Action::Ignore);
        assert_eq!(
            action_of(KeyPress::byte(0x03)),
            Action::Ignore,
            "Ctrl-C is a signal, not a key"
        );
    }

    #[test]
    fn only_ctrl_d_quits_on_windows_too() {
        assert_eq!(action_of(KeyPress::key(VK_D, true)), Action::Quit);
        assert_eq!(
            action_of(KeyPress::key(VK_D, false)),
            Action::Ignore,
            "a bare d is a letter"
        );
        assert_eq!(action_of(KeyPress::key(b'q' as u16, true)), Action::Ignore);
    }
}
