//! Test fixtures for the lib's own tests and for the frontends' tests.
//!
//! The NVENC capability flow is the only part of the lib that shells out
//! to ffmpeg on its own (design doc §3.10.6). Exercising it needs three
//! things the production code otherwise hides:
//!
//! - **an ffmpeg whose answers you decide** -- [`ScriptedFfmpeg`];
//! - **a place to put the capability record** -- [`TempDir`];
//! - **a clock you control** -- [`Clock`].
//!
//! The fixtures drive the **production entry points**
//! (`nvenc::resolve_with_clock`, `nvenc::resolve_and_store_with_clock`,
//! `encoder::select::select_encoder`) -- they never re-implement the flow
//! they test. A double that duplicates the thing it tests is a second
//! implementation of the rules, and it drifts (design doc §3.10.4
//! rule 2).
//!
//! This module is always compiled rather than feature-gated: the lib is
//! internal (`publish = false`), so a public test helper costs nothing
//! in a released artifact, and it lets the integration tests under
//! `tests/` and the frontends' test modules use it without a feature
//! dance.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;

use crate::error::EncodeError;

/// The runner shape the production seams take:
///
/// `Fn(&[&str]) -> Result<String, EncodeError>`.
///
/// It is a `Box` (not an `Arc`): `Box<F>` implements `Fn` when `F`
/// does, `Arc<F>` does not -- and the production seams take the
/// runner by value, so there is nothing to share.
pub type Runner = Box<dyn Fn(&[&str]) -> Result<String, EncodeError> + Send + Sync>;

/// An ffmpeg whose answers you script.
///
/// Every call is recorded, so a test can assert not just the outcome but
/// **what was (not) asked** -- e.g. "a reused record must not trigger a
/// probe".
///
/// ```no_run
/// # use rencoder_lib::test_support::{ScriptedFfmpeg, Outcome};
/// let ffmpeg = ScriptedFfmpeg::new()
///     .encoders(" V..... av1_nvenc  NVIDIA NVENC AV1 encoder\n")
///     .version("ffmpeg version 9.0.1\n")
///     .probe_ok("av1_nvenc");
/// let runner = ffmpeg.runner();
/// // ... hand `runner` to `nvenc::resolve_with_clock`
/// ```
pub struct ScriptedFfmpeg {
    rules: Vec<Rule>,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

/// One scripted answer.
#[derive(Clone)]
struct Rule {
    matcher: Matcher,
    outcome: Outcome,
    /// A `once` rule answers a single call and then goes quiet -- that
    /// is how a test scripts "the first probe fails, the retry works".
    once: bool,
}

/// How a rule recognizes a call.
#[derive(Clone, Debug)]
pub enum Matcher {
    /// The argv is exactly this list (`["-encoders"]`).
    Exact(Vec<String>),
    /// The argv contains this token.
    Contains(String),
    /// The 1-frame capability probe for this encoder (the argv has
    /// `-c:v <encoder>`).
    ProbeOf(String),
}

/// What the script answers with.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// ffmpeg exits 0 and prints this on stdout.
    Stdout(String),
    /// ffmpeg exits 1 with this on stderr.
    Failure(String),
}

impl ScriptedFfmpeg {
    /// A script with no rules: every call fails loudly (see
    /// [`ScriptedFfmpeg::runner`]).
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Add a rule that answers every matching call.
    pub fn on(mut self, matcher: Matcher, outcome: Outcome) -> Self {
        self.rules.push(Rule {
            matcher,
            outcome,
            once: false,
        });
        self
    }

    /// Add a rule that answers a **single** matching call; later calls
    /// fall through to the next rule.
    pub fn once(mut self, matcher: Matcher, outcome: Outcome) -> Self {
        self.rules.push(Rule {
            matcher,
            outcome,
            once: true,
        });
        self
    }

    /// `ffmpeg -encoders` answers with this text.
    pub fn encoders(self, output: impl Into<String>) -> Self {
        self.on(
            Matcher::Exact(vec!["-encoders".into()]),
            Outcome::Stdout(output.into()),
        )
    }

    /// `ffmpeg -version` answers with this text.
    pub fn version(self, output: impl Into<String>) -> Self {
        self.on(
            Matcher::Exact(vec!["-version".into()]),
            Outcome::Stdout(output.into()),
        )
    }

    /// The capability probe for `encoder` succeeds.
    pub fn probe_ok(self, encoder: &str) -> Self {
        self.on(
            Matcher::ProbeOf(encoder.to_string()),
            Outcome::Stdout(String::new()),
        )
    }

    /// The capability probe for `encoder` fails with this stderr text.
    pub fn probe_fail(self, encoder: &str, stderr: &str) -> Self {
        self.on(
            Matcher::ProbeOf(encoder.to_string()),
            Outcome::Failure(stderr.to_string()),
        )
    }

    /// The runner to hand to a production entry point.
    ///
    /// A call that no rule matches is an **error**, not a silent empty
    /// answer: a missing script entry is a bug in the test, and it must
    /// fail loudly instead of looking like "ffmpeg printed nothing".
    pub fn runner(&self) -> Runner {
        let rules = Arc::new(Mutex::new(self.rules.clone()));
        let calls = self.calls.clone();
        Box::new(move |args: &[&str]| {
            let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            calls.lock().push(argv.clone());

            let mut rules = rules.lock();
            let Some(pos) = rules.iter().position(|r| r.matcher.matches(&argv)) else {
                return Err(EncodeError::Probe(format!(
                    "no scripted response for ffmpeg {argv:?}"
                )));
            };
            let outcome = rules[pos].outcome.clone();
            if rules[pos].once {
                // Consume it: the next matching call falls through to the
                // next rule.
                rules.remove(pos);
            }
            outcome_to_result(&outcome)
        })
    }

    /// Every call the runner saw, in order.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().clone()
    }

    /// How many calls the runner saw.
    pub fn call_count(&self) -> usize {
        self.calls.lock().len()
    }

    /// The calls whose argv contains `token`.
    pub fn calls_containing(&self, token: &str) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .iter()
            .filter(|argv| argv.iter().any(|a| a == token))
            .cloned()
            .collect()
    }
}

impl Default for ScriptedFfmpeg {
    fn default() -> Self {
        Self::new()
    }
}

impl Matcher {
    fn matches(&self, argv: &[String]) -> bool {
        match self {
            Matcher::Exact(want) => argv == want.as_slice(),
            Matcher::Contains(token) => argv.iter().any(|a| a == token),
            Matcher::ProbeOf(encoder) => argv
                .windows(2)
                .any(|pair| pair[0] == "-c:v" && pair[1] == *encoder),
        }
    }
}

fn outcome_to_result(outcome: &Outcome) -> Result<String, EncodeError> {
    match outcome {
        Outcome::Stdout(text) => Ok(text.clone()),
        // The same shape a real ffmpeg failure produces, so
        // `nvenc::failure_reason` extracts the stderr the same way.
        Outcome::Failure(stderr) => Err(EncodeError::Probe(format!("ffmpeg exited 1: {stderr}"))),
    }
}

/// A temporary directory that cleans itself up.
///
/// Every instance gets its own directory (label + pid + counter +
/// timestamp), so tests can run in parallel without treading on each
/// other's capability record.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh directory labelled `label`.
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "rencoder_{label}_{}_{n}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory (the file need not exist).
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

/// A fresh, uniquely named temporary directory, as a path.
///
/// The name carries the pid, a counter and the clock, so two tests --
/// or two concurrent test runs -- never share one. A **shared** temp
/// path is a race: one run removes the file the other is about to
/// read, and the failure looks like a product bug.
///
/// Unlike [`TempDir`] there is no guard: the directory stays until
/// the machine cleans the temp dir. These helpers are for the tests
/// that want a plain path, not a lifetime-bound fixture.
pub fn unique_temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        "rencoder_{label}_{}_{n}_{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// A fresh, uniquely named path for a file (the file need not
/// exist). See [`unique_temp_dir`] for why the name must be unique.
pub fn unique_temp_file(label: &str) -> PathBuf {
    unique_temp_dir(label).join("config.toml")
}

/// The portable fake ffmpeg: this test binary, re-entered as a child.
///
/// The cancel tests need a child process that looks like ffmpeg -- it
/// writes a partial output, emits `-progress pipe:1` blocks on stdout
/// and runs until it is killed. A shell script filled that role on
/// Unix, which is why those tests were `#[cfg(unix)]` and never ran on
/// Windows.
///
/// The child is **this very test binary**, re-entered with `--exact
/// <the fake test> --nocapture` plus a marker argument that says where
/// the partial output goes. So there is no second binary to find, no
/// shell to depend on, and **the same test runs on every platform**.
///
/// The output path is **last** in the argv, because that is what the
/// cancel cleanup deletes (design doc §3.11.2).
///
/// # Panics
///
/// Never in a normal test run: without the marker the fake does
/// nothing.
pub fn fake_ffmpeg_argv(output: &Path) -> Vec<String> {
    let exe = std::env::current_exe().expect("the test binary's own path");
    vec![
        exe.to_string_lossy().into_owned(),
        "--exact".into(),
        FAKE_FFMPEG_TEST_NAME.into(),
        "--nocapture".into(),
        // Everything after this is a free argument: libtest stops
        // parsing options there, so the fake's own arguments (and the
        // `-progress pipe:1` the production code appends) do not have
        // to look like options.
        "--".into(),
        FAKE_FFMPEG_MARKER.into(),
        output.to_string_lossy().into_owned(),
    ]
}

/// The marker that tells the fake child where the partial output goes.
///
/// A marker rather than "the last argument": in a normal test run the
/// arguments belong to cargo, and the fake must stay a no-op.
pub const FAKE_FFMPEG_MARKER: &str = "--rencoder-fake-ffmpeg";

/// The full name of the test that acts as the fake ffmpeg child.
pub const FAKE_FFMPEG_TEST_NAME: &str = "test_support::tests::fake_ffmpeg_child";

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A clock the test owns.
///
/// The production code takes "now" as a value (see
/// `nvenc::resolve_with_clock`), so a test drives time by moving this
/// clock and passing `clock.now()` in. `advance` is how a test makes a
/// stored capability record go stale without waiting 30 days.
pub struct Clock {
    at: Mutex<DateTime<Utc>>,
}

impl Clock {
    /// A clock frozen at `at`.
    pub fn fixed(at: DateTime<Utc>) -> Self {
        Self { at: Mutex::new(at) }
    }

    /// A clock frozen at "now".
    pub fn starting_now() -> Self {
        Self::fixed(Utc::now())
    }

    /// The current time.
    pub fn now(&self) -> DateTime<Utc> {
        *self.at.lock()
    }

    /// Move the clock forward.
    pub fn advance(&self, by: Duration) {
        let mut at = self.at.lock();
        *at += by;
    }

    /// Set the clock.
    pub fn set(&self, at: DateTime<Utc>) {
        *self.at.lock() = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_runner_answers_and_records() {
        let ffmpeg = ScriptedFfmpeg::new()
            .encoders("encoders")
            .version("version")
            .probe_ok("av1_nvenc");
        let runner = ffmpeg.runner();

        assert_eq!(runner(&["-encoders"]).unwrap(), "encoders");
        assert_eq!(runner(&["-version"]).unwrap(), "version");
        assert_eq!(runner(&["-c:v", "av1_nvenc", "-f", "null"]).unwrap(), "");

        assert_eq!(ffmpeg.call_count(), 3);
        assert_eq!(ffmpeg.calls_containing("-c:v").len(), 1);
    }

    #[test]
    fn scripted_failure_looks_like_a_real_one() {
        let ffmpeg = ScriptedFfmpeg::new().probe_fail("av1_nvenc", "No NVENC capable devices");
        let runner = ffmpeg.runner();
        let err = runner(&["-c:v", "av1_nvenc"]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("ffmpeg exited 1"), "{text}");
        assert!(text.contains("No NVENC capable devices"), "{text}");
    }

    #[test]
    fn an_unscripted_call_fails_loudly() {
        let ffmpeg = ScriptedFfmpeg::new();
        let runner = ffmpeg.runner();
        let err = runner(&["-encoders"]).unwrap_err();
        assert!(err.to_string().contains("no scripted response"), "{err}");
    }

    #[test]
    fn a_once_rule_answers_only_once() {
        let ffmpeg = ScriptedFfmpeg::new()
            .once(
                Matcher::ProbeOf("av1_nvenc".into()),
                Outcome::Failure("first try fails".into()),
            )
            .probe_ok("av1_nvenc");
        let runner = ffmpeg.runner();

        assert!(runner(&["-c:v", "av1_nvenc"]).is_err());
        assert!(runner(&["-c:v", "av1_nvenc"]).is_ok());
    }

    #[test]
    fn temp_dir_cleans_up_after_itself() {
        let path;
        {
            let dir = TempDir::new("fixture");
            path = dir.path().to_path_buf();
            assert!(path.exists());
        }
        assert!(!path.exists(), "the temp dir must be removed on drop");
    }

    #[test]
    fn clock_moves_only_when_told_to() {
        let start = Utc::now();
        let clock = Clock::fixed(start);
        assert_eq!(clock.now(), start);
        clock.advance(Duration::days(31));
        assert_eq!(clock.now(), start + Duration::days(31));
    }

    /// The fake ffmpeg, as a child process.
    ///
    /// The cancel tests spawn this test binary as the fake (see
    /// [`fake_ffmpeg_argv`]): it writes garbage to the partial output,
    /// emits ffmpeg's `-progress pipe:1` blocks on stdout, and runs
    /// until it is killed.
    ///
    /// In a normal test run there is no marker in the arguments, so
    /// this test does nothing -- it is a no-op, not a fake ffmpeg that
    /// runs for ten seconds.
    #[test]
    fn fake_ffmpeg_child() {
        use std::io::Write;
        use std::time::Duration;

        /// How many progress blocks before the fake gives up on its own.
        const BLOCKS: usize = 200;
        /// The gap between two blocks -- the cadence the cancel poll
        /// sees.
        const INTERVAL: Duration = Duration::from_millis(50);

        let args: Vec<String> = std::env::args().collect();
        let Some(marker) = args.iter().position(|a| a == FAKE_FFMPEG_MARKER) else {
            // Not spawned as the fake: nothing to do.
            return;
        };
        let Some(output) = args.get(marker + 1) else {
            return;
        };

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .expect("the fake ffmpeg writes to the output path");

        for frame in 0..BLOCKS {
            // The shape real ffmpeg prints with `-progress pipe:1`.
            print!("frame={frame}\nfps=30\nspeed=1.0x\nout_time_us=1000000\nprogress=continue\n");
            std::io::stdout().flush().expect("flush stdout");
            let _ = writeln!(file, "garbage");
            let _ = file.flush();
            std::thread::sleep(INTERVAL);
        }
    }
}
