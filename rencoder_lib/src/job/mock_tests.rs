//! End-to-end tests of the StartBatch / Cancel / LoadHistory
//! message protocol without invoking ffmpeg. The actor's encode
//! step is replaced by a closure that returns predetermined
//! EncodingRecord values, so tests verify:
//!
//! - Event sequence (BatchStarted → FileStarted × N → FileCompleted
//!   × N → BatchCompleted).
//! - Single-batch enforcement (second StartBatch while one
//!   running → Event::Error(ConfigError)).
//! - Cancel mid-batch (BatchCancelled with the correct completed
//!   count).
//! - LoadHistory filtering (limit + after) and missing-data-dir
//!   graceful return.
//!
//! Real ffmpeg behavior is NOT tested here — that lives in the
//! `transcode_single(_with_progress)` unit tests. These tests
//! cover the *wiring*.

use super::*;
use crate::types::{EncoderSettings, EncodingOutcome};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

/// Per-process unique counter for tmp dirs in tests. Avoids
/// collisions when tests run in parallel within the same
/// nanosecond.
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmp(label: &str) -> PathBuf {
    let n = TEST_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
    std::env::temp_dir().join(format!("rencoder_mock_{label}_{}_{n}", std::process::id()))
}

/// A records directory that cannot exist -- and therefore cannot
/// be written to, and can never hold records.
///
/// The "no data dir" case needs a path that is guaranteed to be
/// empty. A plain "/nonexistent/..." is not enough: on Windows it
/// resolves against the current drive, and a batch that runs
/// without a data dir *creates* it and writes its records there
/// (`default_encoding_path_in` does a `create_dir_all`) -- so the
/// next test that expects an empty history reads them back, and
/// the suite passes once and fails the next time. Putting a file
/// in the parent keeps the path unusable on every platform.
fn unusable_records_dir() -> PathBuf {
    let dir = unique_tmp("unusable");
    std::fs::create_dir_all(&dir).expect("the scratch dir must exist");
    let blocker = dir.join("not-a-directory");
    std::fs::write(&blocker, b"").expect("the blocker must exist");
    blocker.join("records")
}

/// Test helper: wrap a path literal in `InputPath`. See the
/// equivalent in `mod tests` above for rationale.
fn input(p: &str) -> crate::types::InputPath {
    crate::types::InputPath::new(std::path::Path::new(p)).expect("test fixture")
}

/// A real directory with `count` fake video files in it.
///
/// The actor touches the filesystem: the single-instance guard
/// takes the batch's target directory (design doc §3.11.1.1), so
/// the batch's files must live in a directory that exists.
fn batch_files(label: &str, count: usize) -> Vec<crate::types::InputPath> {
    let dir = unique_tmp(label);
    std::fs::create_dir_all(&dir).expect("the batch dir must exist");
    (0..count)
        .map(|i| input(&dir.join(format!("f{i}.mp4")).to_string_lossy()))
        .collect()
}

/// Run a mock actor with a mock encoder.
///
/// `mock_fn` is called once per file and returns the EncodingRecord
/// for that file. `data_dir` is the path passed to `load_history`
/// (not via env var — parallel-safe).
///
/// The mock mirrors the production actor's structure: batches run
/// on a worker thread and the command loop stays responsive. The
/// tests therefore exercise the real mechanism (§3.10.1.1).
#[allow(clippy::needless_pass_by_value)]
/// Test harness: the **production** actor with test dependencies.
///
/// The mock replaces one step -- "encode one file" -- through the
/// [`FileEncoder`] seam. Everything else (the busy gate, the worker
/// spawn, the event sequences, the cancel wiring) is the
/// production code, so the message protocol has exactly one
/// implementation (design doc §3.10.4).
///
/// `mock_fn` is called once per file and returns the record for
/// that file. `data_dir` is where `Command::SaveConfig` writes and
/// `Command::LoadHistory` reads (not via env var -- parallel-safe).
fn run_test_controller<F>(
    mock_fn: F,
    data_dir: Option<PathBuf>,
) -> (Sender<Request>, Receiver<Event>)
where
    F: Fn(
            &EncodeTask,
            &mut BatchContext,
            &CancelToken,
            &mut dyn FnMut(FfmpegProgress),
        ) -> Result<EncodingRecord, EncodeError>
        + Send
        + Sync
        + 'static,
{
    run_test_controller_with_capability(mock_fn, data_dir, None)
}

/// [`run_test_controller`] with the NVENC capability the cache
/// init hands to the UI.
///
/// The production actor resolves the capability at startup and
/// reports it in an `Event::NvencCapability`; the test harness
/// pre-populates the cache channel instead, so a test that cares
/// about that event (or about the review-date deferral it
/// triggers) passes the capability in here.
fn run_test_controller_with_capability<F>(
    mock_fn: F,
    data_dir: Option<PathBuf>,
    capability: Option<Capability>,
) -> (Sender<Request>, Receiver<Event>)
where
    F: Fn(
            &EncodeTask,
            &mut BatchContext,
            &CancelToken,
            &mut dyn FnMut(FfmpegProgress),
        ) -> Result<EncodingRecord, EncodeError>
        + Send
        + Sync
        + 'static,
{
    run_test_controller_with_runner(mock_fn, data_dir, capability, None)
}

/// [`run_test_controller_with_capability`] with an injected NVENC
/// probe runner.
///
/// The runner is the seam the probe goes through, so a test can
/// make a probe block, or answer differently each time -- the
/// probe's *order* matters (design doc §3.10.6 step 5: an older
/// probe's result must not overwrite a newer one).
#[allow(clippy::too_many_arguments)]
fn run_test_controller_with_runner<F>(
    mock_fn: F,
    data_dir: Option<PathBuf>,
    capability: Option<Capability>,
    detect_runner: Option<crate::job::DetectRunner>,
) -> (Sender<Request>, Receiver<Event>)
where
    F: Fn(
            &EncodeTask,
            &mut BatchContext,
            &CancelToken,
            &mut dyn FnMut(FfmpegProgress),
        ) -> Result<EncodingRecord, EncodeError>
        + Send
        + Sync
        + 'static,
{
    let (cmd_tx, cmd_rx) = unbounded::<Request>();
    let (evt_tx, evt_rx) = unbounded::<Event>();

    // The temp dir plays the records directory: the writer and
    // the history reader must agree on it (design doc §3.6).
    let records_dir = data_dir.clone().unwrap_or_else(unusable_records_dir);
    // The app directory: a temp dir, never the working directory
    // -- the actor writes there (the capability report, the saved
    // config), and a test must not leave files in the source
    // tree.
    let app_dir = data_dir.clone().unwrap_or_else(|| {
        let dir = unique_tmp("app_dir");
        std::fs::create_dir_all(&dir).ok();
        dir
    });
    let config_path = app_dir.join("saved-config.toml");
    let mut paths = Paths::resolve(
        config_path.clone(),
        app_dir.clone(),
        app_dir,
        &AppConfig::default(),
    );
    paths.records_dir = records_dir.clone();
    paths.capability_record = records_dir.join("nvenc-capability.json");

    let env = ActorEnv {
        app_config: Arc::new(AppConfig::default()),
        config_path,
        paths,
        file_encoder: Arc::new(MockFileEncoder { f: mock_fn }),
        // The NVENC probe never runs against real ffmpeg: the
        // fake runner answers it.
        detect_runner: detect_runner.unwrap_or_else(|| scripted_probe_runner(capability.as_ref())),
    };

    std::thread::spawn(move || {
        run_actor(cmd_rx, evt_tx, env);
    });

    (cmd_tx, evt_rx)
}

/// The version line the fake machine reports.
const PROBE_VERSION: &str = "ffmpeg version 6.0 (test)";

/// The fake machine's answers to the capability probe.
///
/// The harness drives the **production** probe path: the runner
/// answers the encoder list, the version, and the 1-frame probes,
/// and the real probe turns those answers into the capability
/// (design doc §3.10.4 rule 2 -- a test double must not
/// re-implement the thing it tests).
fn scripted_probe_runner(capability: Option<&Capability>) -> crate::job::DetectRunner {
    let encoders: Vec<String> = capability
        .map(|c| c.support().iter().map(|e| e.encoder.clone()).collect())
        .unwrap_or_default();
    let usable: Vec<String> = capability
        .map(|c| {
            c.support()
                .iter()
                .filter(|e| e.usable)
                .map(|e| e.encoder.clone())
                .collect()
        })
        .unwrap_or_default();
    let encoders_output = encoders.join("\n");
    Arc::new(move |args: &[&str]| {
        if args.contains(&"-encoders") {
            return Ok(encoders_output.clone());
        }
        if args.contains(&"-version") {
            return Ok(PROBE_VERSION.to_string());
        }
        // A 1-frame capability probe: it succeeds for the encoders
        // this machine can run, and fails for the others.
        let encoder = args
            .windows(2)
            .find(|pair| pair[0] == "-c:v")
            .map(|pair| pair[1].to_string());
        match encoder {
            Some(encoder) if usable.contains(&encoder) => Ok(String::new()),
            Some(_) => Err(EncodeError::Probe(
                "the 1-frame capability probe failed".into(),
            )),
            None => Ok(String::new()),
        }
    })
}

/// The test double for "encode one file".
struct MockFileEncoder<F> {
    f: F,
}

impl<F> FileEncoder for MockFileEncoder<F>
where
    F: Fn(
            &EncodeTask,
            &mut BatchContext,
            &CancelToken,
            &mut dyn FnMut(FfmpegProgress),
        ) -> Result<EncodingRecord, EncodeError>
        + Send
        + Sync
        + 'static,
{
    fn encode(
        &self,
        task: &EncodeTask,
        ctx: &mut BatchContext,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError> {
        (self.f)(task, ctx, cancel, on_progress)
    }
}

/// Build a minimal EncodingRecord marked Succeeded with a given
/// source path. Used by mock encoders to fabricate "completed"
/// records without invoking ffmpeg.
fn fake_record(path: &crate::types::InputPath) -> EncodingRecord {
    EncodingRecord {
        source_path: path.to_string(),
        source_size_bytes: 0,
        source_codec: None,
        source_container: None,
        source_width: None,
        source_height: None,
        source_fps: None,
        source_duration_secs: None,
        source_bit_rate_bps: None,
        started_at: chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        ended_at: Some(chrono::DateTime::<Utc>::from_timestamp(1_700_000_010, 0).unwrap()),
        encoding_duration_secs: Some(10.0),
        encoder_selected: None,
        encoder_settings: EncoderSettings::default(),
        ffmpeg_argv: vec![],
        ffmpeg_stderr_tail: String::new(),
        output_path: None,
        output_size_bytes: None,
        output_duration_secs: None,
        output_codec: None,
        compression_ratio: None,
        outcome: EncodingOutcome::Succeeded,
        error: None,
    }
}

/// Mock encoder that returns a Succeeded record for each file
/// and counts calls. No progress events emitted.
fn make_succeeding_mock() -> (
    impl Fn(
        &EncodeTask,
        &mut BatchContext,
        &CancelToken,
        &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError>
    + Send
    + Sync
    + 'static,
    Arc<AtomicUsize>,
) {
    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = count.clone();
    let f = move |task: &EncodeTask,
                  _: &mut BatchContext,
                  _: &CancelToken,
                  _: &mut dyn FnMut(FfmpegProgress)|
          -> Result<EncodingRecord, EncodeError> {
        count_clone.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(fake_record(&task.input))
    };
    (f, count)
}

/// Mock encoder that emits 2 progress blocks per file then
/// returns Succeeded. Used to verify Event::Progress flow.
fn make_progress_mock() -> impl Fn(
    &EncodeTask,
    &mut BatchContext,
    &CancelToken,
    &mut dyn FnMut(FfmpegProgress),
) -> Result<EncodingRecord, EncodeError>
+ Send
+ Sync
+ 'static {
    fn f(
        task: &EncodeTask,
        _: &mut BatchContext,
        _: &CancelToken,
        on_progress: &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError> {
        on_progress(FfmpegProgress {
            frame: 100,
            fps: 30.0,
            speed: 1.0,
            time_secs: 5.0,
            eta_secs: Some(55.0),
        });
        on_progress(FfmpegProgress {
            frame: 200,
            fps: 30.0,
            speed: 1.0,
            time_secs: 10.0,
            eta_secs: Some(50.0),
        });
        Ok(fake_record(&task.input))
    }
    f
}

/// The events of a batch, without the actor's own publications.
///
/// The capability publication is the actor's business -- it lands
/// when the first probe finishes. The batch's event sequence is
/// what these tests are about, so the publications are filtered
/// out and the sequence is asserted on its own.
fn batch_events(events: Vec<Event>) -> Vec<Event> {
    events
        .into_iter()
        .filter(|e| {
            !matches!(
                e,
                Event::NvencCapability(_) | Event::CapabilityChecked { .. }
            )
        })
        .collect()
}

/// The safety net for every wait in this module.
///
/// A timeout is **not** a synchronization: it must never be the
/// thing a test relies on to mean "it happened". The waits below
/// return as soon as the event arrives; the timeout only bounds
/// a test that would otherwise hang forever.
const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Wait for the next event that satisfies `pred`.
///
/// The event arrives in milliseconds; waiting for a fixed
/// window instead would make the test slow **and** fragile (a
/// loaded machine can miss the window).
fn wait_for_event<F>(rx: &Receiver<Event>, pred: F) -> Event
where
    F: Fn(&Event) -> bool,
{
    let deadline = std::time::Instant::now() + WAIT_LIMIT;
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("timed out waiting for the event");
        match rx.recv_timeout(remaining) {
            Ok(event) => {
                if pred(&event) {
                    return event;
                }
            }
            Err(e) => panic!("timed out waiting for the event: {e}"),
        }
    }
}

/// Wait for the batch's **terminal** event and return everything
/// that arrived up to it.
///
/// The terminal event *is* the "this batch is over" signal
/// (§3.11.4): one batch, one terminal event. So a test that
/// wants "what this batch did" waits for that -- not for a
/// window.
///
/// This is the **barrier's release point** (design doc §3.11.3):
/// the stage publishes, the next stage starts. The tests wait
/// for the publication, exactly as the chain does.
fn batch_events_until_end(rx: &Receiver<Event>) -> Vec<Event> {
    let mut events = Vec::new();
    let deadline = std::time::Instant::now() + WAIT_LIMIT;
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("timed out waiting for the batch to end");
        match rx.recv_timeout(remaining) {
            Ok(event) => {
                let terminal = is_terminal_event(&event);
                events.push(event);
                if terminal {
                    return events;
                }
            }
            Err(e) => panic!("timed out waiting for the batch to end: {e}"),
        }
    }
}

/// Assert that no event satisfying `pred` arrives within
/// `window`.
///
/// A negative wait **is** a window -- there is nothing else to
/// wait for. It cannot produce a false failure (a late event only
/// makes the assertion weaker), so the window can be modest.
fn assert_no_event<F>(rx: &Receiver<Event>, pred: F, window: std::time::Duration)
where
    F: Fn(&Event) -> bool,
{
    let deadline = std::time::Instant::now() + window;
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(event) => {
                assert!(!pred(&event), "an unexpected event arrived: {event:?}");
            }
            Err(_) => break,
        }
    }
}

/// The next capability publication, skipping the other events.
fn next_capability_event(rx: &Receiver<Event>) -> Capability {
    match wait_for_event(rx, |e| matches!(e, Event::NvencCapability(_))) {
        Event::NvencCapability(capability) => capability,
        other => unreachable!("the predicate matched: {other:?}"),
    }
}

/// Is this event the end of a batch?
fn is_terminal_event(event: &Event) -> bool {
    matches!(
        event,
        Event::BatchCompleted { .. }
            | Event::BatchCancelled { .. }
            | Event::BatchFailed { .. }
            | Event::DryRunFinished { .. }
            | Event::AnalyzeFinished { .. }
    )
}

#[test]
fn a_busy_target_is_a_benign_rejection() {
    // Another instance holds the target (design doc §3.11.1.1):
    // the command is **rejected**, not an error -- the running
    // batch is untouched, and this one never starts.
    let files = batch_files("busy_target", 1);
    let target = files[0]
        .as_path()
        .parent()
        .expect("the file has a parent")
        .to_path_buf();
    // Take the target the way another instance would.
    let _held = crate::guard::TargetGuard::acquire(&target).expect("the target is free");

    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CommandRejected { .. }));
    let events = [event];
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::CommandRejected { .. })),
        "the busy target must be a rejection, got {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "a rejected command is not a fatal error: {events:?}"
    );
}

#[test]
fn a_busy_descendant_stops_the_batch() {
    // A directory **inside** the batch's scope is held by another
    // instance (design doc §3.11.1.1): the batch cannot start --
    // it would collect that directory's files and write the same
    // outputs.
    let root = unique_tmp("busy_descendant");
    let held_dir = root.join("held");
    let other_dir = root.join("other");
    std::fs::create_dir_all(&held_dir).unwrap();
    std::fs::create_dir_all(&other_dir).unwrap();
    // Another instance holds one of the two directories.
    let _held = crate::guard::TargetGuard::acquire(&held_dir).expect("the target is free");
    // This batch works on both -- its scope is their common parent.
    let files = vec![
        input(&held_dir.join("a.mp4").to_string_lossy()),
        input(&other_dir.join("b.mp4").to_string_lossy()),
    ];

    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BatchFailed { .. })),
        "the batch must not start: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::BatchCompleted { .. })),
        "no batch may run on a busy scope: {events:?}"
    );
}

#[test]
fn start_batch_emits_expected_event_sequence() {
    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
    let files = batch_files("event_sequence", 3);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: files.clone(),
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let events = batch_events(batch_events_until_end(&evt_rx));
    // Expected sequence: per-file events interleave (each
    // process_one_file emits FileStarted then FileCompleted
    // before moving to the next file).
    //   CommandAccepted
    //   BatchStarted
    //   FileStarted(0) FileCompleted(0)
    //   FileStarted(1) FileCompleted(1)
    //   FileStarted(2) FileCompleted(2)
    //   BatchCompleted
    assert_eq!(events.len(), 2 + 3 * 2 + 1);

    // The request is answered before the batch starts (design
    // doc §3.11.3): the answer carries the batch id.
    let batch_id = match &events[0] {
        Event::CommandAccepted {
            request_id,
            batch_id: Some(id),
        } => {
            assert_eq!(*request_id, 1, "the answer carries the request id");
            *id
        }
        other => panic!("expected CommandAccepted first, got {other:?}"),
    };
    assert!(matches!(
        events[1],
        Event::BatchStarted { total_files: 3, .. }
    ));

    // Verify per-file pairs in order.
    for i in 0..3 {
        let started = &events[2 + i * 2];
        let completed = &events[2 + i * 2 + 1];
        match (started, completed) {
            (
                Event::FileStarted {
                    batch_id: sb,
                    index: si,
                    total: st,
                    source: ss,
                },
                Event::FileCompleted {
                    batch_id: cb,
                    index: ci,
                    ..
                },
            ) => {
                assert_eq!(*si, i);
                assert_eq!(*ci, i);
                assert_eq!(*st, 3);
                assert_eq!(ss, &files[i]);
                // Every event of the batch carries its id (design
                // doc §3.11.3).
                assert_eq!(*sb, batch_id);
                assert_eq!(*cb, batch_id);
            }
            _ => panic!("events {started:?} {completed:?} not a started/completed pair"),
        }
    }

    match events.last().unwrap() {
        Event::BatchCompleted {
            batch_id: id,
            records,
        } => {
            assert_eq!(*id, batch_id);
            assert_eq!(records.len(), 3);
            for (i, r) in records.iter().enumerate() {
                assert_eq!(r.source_path, files[i].to_string());
            }
        }
        _ => panic!("expected BatchCompleted as last event"),
    }
}

#[test]
fn start_batch_emits_progress_events_during_encode() {
    let (cmd_tx, evt_rx) = run_test_controller(make_progress_mock(), None);
    let files = batch_files("progress", 2);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let events = batch_events(batch_events_until_end(&evt_rx));
    // Expected: CommandAccepted + BatchStarted
    //   + (FileStarted + Progress×2 + FileCompleted) × 2 + BatchCompleted
    assert_eq!(events.len(), 2 + (1 + 2 + 1) * 2 + 1);
    let progress_count = events
        .iter()
        .filter(|e| matches!(e, Event::Progress { .. }))
        .count();
    assert_eq!(progress_count, 4); // 2 per file × 2 files
}

#[test]
fn start_batch_rejects_second_batch_with_error() {
    // The busy gate covers the worker's lifetime: the second
    // StartBatch arrives while the first batch is still running,
    // so it must be rejected instead of queued.
    //
    // The mock sleeps long enough that the first batch is
    // definitely still running when the second command is
    // processed.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let (cmd_tx, evt_rx) = run_test_controller(
        move |task, _, _, _| {
            calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(fake_record(&task.input))
        },
        None,
    );
    let files = batch_files("second_batch", 1);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: files.clone(),
            },
        ))
        .unwrap();
    cmd_tx
        .send(Request::new(
            2,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();

    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::CommandRejected { reason, .. } if reason.contains("another batch")
        )),
        "the second batch must be rejected, got: {events:?}"
    );
    // Only the first batch ran (one file).
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
}

#[test]
fn cancel_command_stops_a_running_batch() {
    // Regression test for the actor/worker split (§3.10.1.1).
    //
    // Before the split the actor ran the batch inline, so
    // `Command::Cancel` sat in the command queue until the batch
    // was over — the UI's cancel button never aborted anything.
    // Now the batch runs on a worker and the cancel reaches it.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let (cmd_tx, evt_rx) = run_test_controller(
        move |task, _, _, _| {
            calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
            // Slow enough that the cancel lands while this file
            // is still being encoded.
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(fake_record(&task.input))
        },
        None,
    );
    let files: Vec<crate::types::InputPath> = batch_files("cancel_running", 20);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();

    // Wait until the worker really started the batch -- and
    // learn the batch id (the cancel names it, design doc
    // §3.11.3).
    let batch_id = loop {
        match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Event::BatchStarted { id, .. }) => break id,
            Ok(_) => continue,
            Err(e) => panic!("no BatchStarted event: {e}"),
        }
    };

    cmd_tx
        .send(Request::new(2, Command::Cancel { batch_id }))
        .unwrap();

    let events = batch_events(batch_events_until_end(&evt_rx));
    let last = events.last().expect("expected a terminal event");
    match last {
        Event::BatchCancelled {
            batch_id: id,
            completed,
            total,
        } => {
            assert_eq!(*id, batch_id);
            assert_eq!(*total, 20);
            assert!(
                *completed < 20,
                "the batch must stop early, completed={completed}"
            );
        }
        _ => panic!("expected BatchCancelled, got {last:?}"),
    }
    // The encoder must not have run through the whole list.
    let calls = calls.load(AtomicOrdering::SeqCst);
    assert!(
        calls < 20,
        "cancel must stop the batch, but {calls} files were encoded"
    );
}

#[test]
fn cancel_mid_batch_emits_batch_cancelled() {
    // Mock encoder that cancels the token on the 3rd call (after
    // 2 records have been emitted).
    let (cmd_tx, evt_rx) = run_test_controller(
        |task, _, cancel, _| {
            cancel.cancel();
            Ok(fake_record(&task.input))
        },
        None,
    );
    let files: Vec<crate::types::InputPath> = batch_files("cancel_mid", 5);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let events = batch_events(batch_events_until_end(&evt_rx));
    // First file completes (FileStarted + FileCompleted) before
    // cancel propagates; then BatchCancelled with completed = 1.
    // (Mock cancels on file 1 itself, but the file's FileCompleted
    // already emitted before the cancel was observed.)
    let last = events.last().unwrap();
    match last {
        Event::BatchCancelled {
            completed, total, ..
        } => {
            assert_eq!(*completed, 1);
            assert_eq!(*total, 5);
        }
        _ => panic!("expected BatchCancelled, got {last:?}"),
    }
}

#[test]
fn the_batch_start_is_announced_when_the_worker_starts() {
    // The chain's first stage is a barrier: the batch is admitted
    // first and only then waits for the facts. `BatchStarted`
    // says "the worker is running" -- so it must not fire before
    // the facts land (design doc §3.10.1.2 / §3.11.3).
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_for_runner = Arc::clone(&release);
    let runner: crate::job::DetectRunner = Arc::new(move |_args: &[&str]| {
        // The facts never land until the test lets them.
        while !release_for_runner.load(AtomicOrdering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(String::new())
    });
    let (cmd_tx, evt_rx) = run_test_controller_with_runner(
        |task: &EncodeTask,
         _: &mut BatchContext,
         _: &CancelToken,
         _: &mut dyn FnMut(FfmpegProgress)| Ok(fake_record(&task.input)),
        None,
        None,
        Some(runner),
    );

    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("start_announcement", 1),
            },
        ))
        .unwrap();

    // The batch is admitted -- but the facts have not landed, so
    // the worker has not started: no announcement yet.
    wait_for_event(&evt_rx, |e| matches!(e, Event::CommandAccepted { .. }));
    assert_no_event(
        &evt_rx,
        |e| matches!(e, Event::BatchStarted { .. }),
        std::time::Duration::from_millis(200),
    );

    // The facts land: the worker starts, and the batch is
    // announced.
    release.store(true, AtomicOrdering::SeqCst);
    let started = wait_for_event(&evt_rx, |e| matches!(e, Event::BatchStarted { .. }));
    assert!(
        matches!(started, Event::BatchStarted { total_files: 1, .. }),
        "the announcement names the batch's files: {started:?}"
    );
}

#[test]
fn a_cancel_interrupts_the_wait_for_the_facts() {
    // The chain's barrier can be interrupted (design doc §3.10.6):
    // a batch that waits for the facts and is cancelled ends
    // **there** -- the worker is never started, and the batch does
    // not sit in the wait until the facts land.
    //
    // The facts never land here: the probe waits for the test. That
    // is the point -- the wait used to be the worker's, and the
    // cancel could not reach it.
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_for_runner = Arc::clone(&release);
    let runner: crate::job::DetectRunner = Arc::new(move |_args: &[&str]| {
        while !release_for_runner.load(AtomicOrdering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(String::new())
    });
    let (cmd_tx, evt_rx) = run_test_controller_with_runner(
        |task: &EncodeTask,
         _: &mut BatchContext,
         _: &CancelToken,
         _: &mut dyn FnMut(FfmpegProgress)| Ok(fake_record(&task.input)),
        None,
        None,
        Some(runner),
    );

    // The batch: admitted, waiting at the barrier.
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("cancel_at_barrier", 1),
            },
        ))
        .unwrap();
    let batch_id = match wait_for_event(&evt_rx, |e| {
        matches!(
            e,
            Event::CommandAccepted {
                batch_id: Some(_),
                ..
            }
        )
    }) {
        Event::CommandAccepted { batch_id, .. } => batch_id.expect("the batch has an id"),
        other => unreachable!("the predicate matched: {other:?}"),
    };

    // The cancel arrives while the batch waits for the facts.
    cmd_tx
        .send(Request::new(2, Command::Cancel { batch_id }))
        .unwrap();

    // The batch ends at once.
    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::BatchCancelled { .. }));
    match event {
        Event::BatchCancelled {
            completed, total, ..
        } => {
            assert_eq!(completed, 0, "no file started");
            assert_eq!(total, 1, "the batch had one file");
        }
        other => panic!("expected BatchCancelled, got {other:?}"),
    }

    // Let the probe out: it publishes into a batch that is gone.
    release.store(true, AtomicOrdering::SeqCst);
}

#[test]
fn a_cancel_that_names_another_batch_is_refused() {
    // Design doc §3.11.3: only the active batch may be cancelled.
    // A stale cancel (the user cancelled, then a new batch
    // started) must not stop the batch that is running now.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let (cmd_tx, evt_rx) = run_test_controller(
        move |task, _, _, _| {
            calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(fake_record(&task.input))
        },
        None,
    );
    let files: Vec<crate::types::InputPath> = batch_files("stale_cancel", 6);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();

    // Wait for the batch to announce its id.
    let batch_id = loop {
        match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Event::BatchStarted { id, .. }) => break id,
            Ok(_) => continue,
            Err(e) => panic!("no BatchStarted event: {e}"),
        }
    };

    // A cancel for a different batch: refused.
    cmd_tx
        .send(Request::new(
            2,
            Command::Cancel {
                batch_id: batch_id + 100,
            },
        ))
        .unwrap();

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CommandRejected { .. }));
    let events = [event];
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::CommandRejected { request_id: 2, .. })),
        "a cancel for another batch must be refused: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::BatchCancelled { .. })),
        "the running batch must not be cancelled: {events:?}"
    );
}

#[test]
fn a_rejected_request_names_the_request() {
    // The busy gate's refusal says *which* request was refused
    // (design doc §3.11.3): the UI restores the state of that
    // request, not of "the last one".
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let (cmd_tx, evt_rx) = run_test_controller(
        move |task, _, _, _| {
            calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(fake_record(&task.input))
        },
        None,
    );
    let files = batch_files("reject_names", 1);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: files.clone(),
            },
        ))
        .unwrap();
    cmd_tx
        .send(Request::new(
            77,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CommandRejected { .. }));
    let events = [event];
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::CommandRejected { request_id: 77, .. })),
        "the refusal must name the refused request: {events:?}"
    );
}

#[test]
fn load_history_returns_empty_when_no_data_dir() {
    // No RENENCODER_DATA_DIR set + no resolvable app dir →
    // load_history returns Vec::new() and emits HistoryLoaded(empty).
    //
    // SAFETY: `remove_var` is unsafe in a multithreaded program
    // (edition 2024 makes that explicit) because another thread could
    // be reading the environment at the same time. This test binary
    // runs one test at a time here, and no other thread in this
    // process reads the environment.
    unsafe {
        std::env::remove_var("RENENCODER_DATA_DIR");
    }
    let (cmd_tx, evt_rx) = run_test_controller(|task, _, _, _| Ok(fake_record(&task.input)), None);
    cmd_tx
        .send(Request::new(
            1,
            Command::LoadHistory {
                limit: None,
                after: None,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    // The history is not a batch: it waits for **its own**
    // publication (the barrier's release point for that step).
    let loaded = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
    match loaded {
        Event::HistoryLoaded { records } => assert!(records.is_empty()),
        other => panic!("expected HistoryLoaded, got {other:?}"),
    }
}

#[test]
fn load_history_reads_parquet_files_in_data_dir() {
    // Write 3 records to a temp parquet file, send LoadHistory,
    // expect HistoryLoaded with those records (newest-first by
    // started_at; same timestamp here so order is implementation
    // detail — just check the set).
    let tmp = unique_tmp("load");
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("encoding-test.parquet");

    let records: Vec<EncodingRecord> = (0..3)
        .map(|i| {
            let mut r = fake_record(&input(&format!("/lh/{i}.mp4")));
            r.started_at =
                chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + i as i64 * 10, 0).unwrap();
            r
        })
        .collect();
    let bytes = crate::data::store::records_to_parquet(&records).expect("parquet");
    std::fs::write(&path, bytes).expect("write parquet");

    let (cmd_tx, evt_rx) = run_test_controller(
        |task, _, _, _| Ok(fake_record(&task.input)),
        Some(tmp.clone()),
    );
    cmd_tx
        .send(Request::new(
            1,
            Command::LoadHistory {
                limit: None,
                after: None,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    // The history is not a batch: it waits for **its own**
    // publication (the barrier's release point for that step).
    let loaded = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
    match loaded {
        Event::HistoryLoaded { records: loaded } => {
            assert_eq!(loaded.len(), 3);
            // Newest-first ordering.
            assert!(loaded[0].started_at >= loaded[1].started_at);
            assert!(loaded[1].started_at >= loaded[2].started_at);
        }
        other => panic!("expected HistoryLoaded, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn a_finished_batch_writes_its_records_to_the_history_file() {
    // The GUI's history reads `<data_dir>/encoding-*.parquet`; if
    // nobody writes it, the history stays empty forever (design
    // doc §3.11.4).
    let dir = unique_tmp("batch_records");
    std::fs::create_dir_all(&dir).unwrap();

    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, Some(dir.clone()));
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("history", 1),
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BatchCompleted { .. })),
        "expected a BatchCompleted event, got {events:?}"
    );

    // The records are on disk, under the name the history reader
    // recognises.
    let written: Vec<_> = std::fs::read_dir(&dir)
        .expect("the data dir must be readable")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("encoding-") && name.ends_with(".parquet"))
        .collect();
    assert_eq!(
        written.len(),
        1,
        "the batch's records must be written: {written:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "parallel-analyze")]
#[test]
fn save_config_persists_the_caller_s_config() {
    // The payload is the user's edited draft. If the actor
    // persisted its own copy instead, the edit would be lost --
    // the file would be rewritten with the original values.
    let dir = unique_tmp("save_config");
    std::fs::create_dir_all(&dir).unwrap();

    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, Some(dir.clone()));
    let edited = AppConfig {
        encode: Some(crate::config::app::EncodeConfig {
            crf: 28,
            ..Default::default()
        }),
        ..Default::default()
    };
    cmd_tx
        .send(Request::new(
            1,
            Command::SaveConfig {
                config: Box::new(edited),
                revision: 7,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::ConfigSaved { .. }));
    let events = [event];
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ConfigSaved { revision: 7, .. })),
        "the receipt must carry the revision it saved, got {events:?}"
    );

    let written = std::fs::read_to_string(dir.join("saved-config.toml")).unwrap();
    assert!(
        written.contains("crf = 28"),
        "the edited value must reach the file: {written}"
    );
}

#[test]
fn a_capability_check_emits_a_capability_event() {
    // The actor's Command::CheckCapability arm: the capability comes back
    // as an event so the UI can render it.
    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
    cmd_tx
        .send(Request::new(
            1,
            Command::CheckCapability {
                ignore_record: true,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::NvencCapability(_)));
    let events = [event];
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::NvencCapability(_))),
        "expected a NvencCapability event, got {events:?}"
    );
}

#[test]
fn a_working_directory_change_rechecks_and_writes_the_report() {
    // The GUI's working directory moved: the operation directory
    // follows it (design doc §3.6), the capability check runs
    // again (design doc §3.10.7), and the report lands in the new
    // operation directory.
    let app = unique_tmp("working_dir_app");
    let working = unique_tmp("working_dir_op");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&working).unwrap();
    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, Some(app.clone()));
    cmd_tx
        .send(Request::new(
            1,
            Command::SetWorkingDir {
                dir: working.clone(),
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CapabilityChecked { .. }));
    let events = [event];
    let report = events
        .iter()
        .find_map(|e| match e {
            Event::CapabilityChecked { report } => Some(report.clone()),
            _ => None,
        })
        .expect("the check must announce its report");
    assert_eq!(
        report,
        working.join("capabilities.json"),
        "the report belongs to the new operation directory"
    );
    assert!(
        report.exists(),
        "the report must be written: {}",
        report.display()
    );
    let _ = std::fs::remove_dir_all(&app);
    let _ = std::fs::remove_dir_all(&working);
}

#[test]
fn analyze_emits_started_progress_finished_sequence() {
    // U22 / Tier 3 #9: send Command::Analyze with 3 temp files
    // (random bytes — they fail to probe, but the actor still
    // emits the full Started + N×Progress + Finished event
    // sequence). Verifies the actor's Command::Analyze arm.
    //
    // The closure-injection semantics of
    // analyze_records_with_probe_and_progress_parallel (per-file
    // tick, monotonic done, FileOutcome variants) are tested
    // separately in analyzer::records::tests — this test only
    // pins the actor's emit sequence.
    let tmp = unique_tmp("analyze");
    std::fs::create_dir_all(&tmp).unwrap();
    let files: Vec<crate::types::InputPath> = (0..3)
        .map(|i| {
            let p = tmp.join(format!("f{i}.mp4"));
            std::fs::write(&p, b"not a real video").unwrap();
            crate::types::InputPath::new(&p).unwrap()
        })
        .collect();

    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
    cmd_tx
        .send(Request::new(
            1,
            Command::Analyze {
                files: files.clone(),
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let events = batch_events(batch_events_until_end(&evt_rx));
    // Expected: 1 CommandAccepted + 1 AnalyzeStarted
    //            + 3 AnalyzeProgress + 1 AnalyzeFinished.
    assert_eq!(events.len(), 6, "expected 6 events, got: {events:?}");
    // First event: the request is answered (design doc §3.11.3).
    match &events[0] {
        Event::CommandAccepted { batch_id, .. } => assert!(batch_id.is_some()),
        _ => panic!("expected CommandAccepted first, got: {:?}", events[0]),
    }
    // Second event: AnalyzeStarted with total = 3.
    match &events[1] {
        Event::AnalyzeStarted { total, .. } => assert_eq!(*total, 3),
        _ => panic!("expected AnalyzeStarted, got: {:?}", events[1]),
    }
    // Middle 3 events: AnalyzeProgress, all with total = 3.
    for evt in &events[2..5] {
        match evt {
            Event::AnalyzeProgress { total, last, .. } => {
                assert_eq!(*total, 3);
                assert!(last.is_some(), "every Progress tick should carry last");
            }
            _ => panic!("expected AnalyzeProgress, got: {evt:?}"),
        }
    }
    // Last event: AnalyzeFinished with 3 records in input order
    // and failed_count = 3 (all probes fail on garbage files).
    match events.last().unwrap() {
        Event::AnalyzeFinished {
            records,
            failed_count,
            ..
        } => {
            assert_eq!(*failed_count, 3);
            assert_eq!(records.len(), 3);
            // Input order: f0, f1, f2 (par_iter().collect() preserves).
            for (i, r) in records.iter().enumerate() {
                assert_eq!(r.path, files[i].as_path());
                assert!(r.fps.is_none(), "garbage file → fps = None");
            }
        }
        _ => panic!("expected AnalyzeFinished last, got: {:?}", events.last()),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(feature = "parallel-analyze")]
#[test]
fn analyze_runs_to_completion() {
    // The Analyze success path: a normal batch runs to completion
    // (Started + Progress + Finished) with the mocked-out probe
    // backend.
    //
    // (This test used to be named
    // `analyze_rejects_concurrent_batch_with_error`, but it never
    // tested the rejection: the mock's Analyze arm had no busy
    // gate. The mock now drives the production actor, which does
    // gate Analyze -- see `start_batch_rejects_second_batch_with_error`
    // for the gate contract.)
    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
    let tmp = unique_tmp("analyze-busy");
    std::fs::create_dir_all(&tmp).unwrap();
    let files: Vec<crate::types::InputPath> = (0..3)
        .map(|i| {
            let p = tmp.join(format!("f{i}.mp4"));
            std::fs::write(&p, b"not a real video").unwrap();
            crate::types::InputPath::new(&p).unwrap()
        })
        .collect();
    cmd_tx
        .send(Request::new(
            1,
            Command::Analyze {
                files: files.clone(),
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.
    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(matches!(events[1], Event::AnalyzeStarted { .. }));
    assert!(matches!(
        events.last().unwrap(),
        Event::AnalyzeFinished { .. }
    ));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(feature = "parallel-analyze")]
#[test]
fn analyze_empty_files_emits_started_then_finished() {
    // U22: zero-file analyze batch — production arm emits
    // Started unconditionally first, then the parallel loop
    // runs zero times (rayon par_iter is a no-op on empty
    // slices), then Finished fires. Sequence: Started +
    // Finished (2 events). Lets the UI clear any stale
    // "analyzing" state. Test name (U22 review-delta #11)
    // now matches the actual expectation; the previous
    // "emits_finished_only" was a lie since the body asserted
    // 2 events.
    let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
    cmd_tx
        .send(Request::new(1, Command::Analyze { files: Vec::new() }))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let events = batch_events(batch_events_until_end(&evt_rx));
    assert_eq!(
        events.len(),
        3,
        "expected Accepted + Started + Finished, got: {events:?}"
    );
    match &events[1] {
        Event::AnalyzeStarted { total, .. } => assert_eq!(*total, 0),
        _ => panic!("expected AnalyzeStarted"),
    }
    match &events[2] {
        Event::AnalyzeFinished {
            records,
            failed_count,
            ..
        } => {
            assert!(records.is_empty());
            assert_eq!(*failed_count, 0);
        }
        _ => panic!("expected AnalyzeFinished"),
    }
}

#[test]
fn load_history_applies_limit_filter() {
    let tmp = unique_tmp("limit");
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("encoding-limit.parquet");
    let records: Vec<EncodingRecord> = (0..10)
        .map(|i| {
            let mut r = fake_record(&input(&format!("/lim/{i}.mp4")));
            r.started_at =
                chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + i as i64, 0).unwrap();
            r
        })
        .collect();
    let bytes = crate::data::store::records_to_parquet(&records).expect("parquet");
    std::fs::write(&path, bytes).expect("write");

    let (cmd_tx, evt_rx) = run_test_controller(
        |task, _, _, _| Ok(fake_record(&task.input)),
        Some(tmp.clone()),
    );
    cmd_tx
        .send(Request::new(
            1,
            Command::LoadHistory {
                limit: Some(3),
                after: None,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
    let events = [event];
    match &events[0] {
        Event::HistoryLoaded { records: loaded } => {
            assert_eq!(loaded.len(), 3);
            // Newest-first: highest started_at first.
            assert!(loaded[0].started_at.timestamp() > loaded[1].started_at.timestamp());
            assert!(loaded[1].started_at.timestamp() > loaded[2].started_at.timestamp());
        }
        _ => panic!("expected HistoryLoaded"),
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn load_history_applies_after_filter() {
    let tmp = unique_tmp("after");
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("encoding-after.parquet");
    let records: Vec<EncodingRecord> = (0..5)
        .map(|i| {
            let mut r = fake_record(&input(&format!("/aft/{i}.mp4")));
            r.started_at =
                chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + i as i64 * 100, 0).unwrap();
            r
        })
        .collect();
    let bytes = crate::data::store::records_to_parquet(&records).expect("parquet");
    std::fs::write(&path, bytes).expect("write");

    let (cmd_tx, evt_rx) = run_test_controller(
        |task, _, _, _| Ok(fake_record(&task.input)),
        Some(tmp.clone()),
    );
    let cutoff = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + 200, 0).unwrap();
    cmd_tx
        .send(Request::new(
            1,
            Command::LoadHistory {
                limit: None,
                after: Some(cutoff),
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
    let events = [event];
    match &events[0] {
        Event::HistoryLoaded { records: loaded } => {
            // started_at values: 0, 100, 200, 300, 400 (seconds offset).
            // after = 200 → keep records with started_at >= 200.
            assert_eq!(loaded.len(), 3);
            for r in loaded {
                assert!(r.started_at >= cutoff);
            }
        }
        _ => panic!("expected HistoryLoaded"),
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn a_per_file_failure_travels_in_the_record_not_in_a_fatal_event() {
    // Design doc §3.11.3: a file failure belongs to the file. It
    // travels in the record's `error` field -- a fatal event
    // would finalize the whole batch while the other files are
    // still running.
    let (cmd_tx, evt_rx) = run_test_controller(
        |_, _, _, _| {
            Err(crate::error::EncodeError::Ffmpeg(
                "simulated failure".into(),
            ))
        },
        None,
    );
    let files = batch_files("encode_error", 1);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    // The batch's events -- up to its terminal event (the
    // barrier's release point).
    let events = batch_events(batch_events_until_end(&evt_rx));
    // A per-file failure is **not** a fatal event (design doc
    // §3.11.3): it travels in the record's `error` field.
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "a file failure is not a fatal event: {events:?}"
    );
    // Review #120: failed records ARE included in the final
    // BatchCompleted list (the per-file failure is mapped to
    // Ok(Failed record) inside the processor closure, not to an
    // aborted batch).
    match events.last().unwrap() {
        Event::BatchCompleted { records, .. } => {
            assert_eq!(records.len(), 1);
            assert!(matches!(records[0].outcome, EncodingOutcome::Failed));
            assert!(records[0].error.is_some(), "the record carries the error");
            assert!(matches!(
                records[0].error.as_ref().unwrap().category,
                ErrorCategory::FfmpegSpawnFailed
            ));
        }
        other => panic!("expected BatchCompleted, got {other:?}"),
    }
}

#[test]
fn cancel_during_encode_removes_partial_output() {
    // v1.2 follow-up #2: end-to-end test through the actor's
    // full StartBatch path. The mock calls the REAL
    // run_ffmpeg_with_progress with this test binary as the
    // fake-ffmpeg (test_support::fake_ffmpeg_argv): it writes to
    // a real partial output file, and emits `progress=continue`
    // blocks like real ffmpeg.
    //
    // On the first progress block, the mock cancels the
    // token. The next cancel poll in run_ffmpeg_with_progress
    // fires (within ~100ms), kills the child, runs the
    // cleanup helper, and returns Err. The actor's processor
    // closure wraps the err into a Failed record; TaskManager
    // moves to the next file (where the between-files cancel
    // poll in job.rs fires and aborts the batch).
    //
    // After the actor drains, the partial output file must
    // be GONE — proving the cleanup ran in the real flow,
    // not just my isolated unit test.
    //
    // NOTE: `run_test_controller` (job.rs:1355) ignores the
    // `params` from `Command::StartBatch` and uses its own
    // `EncodeParams::default()` (X264, default ext = "mkv"
    // after v1.2 follow-up #3). So `task.output = file1.mkv`
    // — different from the input (`file1.mp4`). The mock must
    // use `task.output` as argv's last element for cleanup to
    // hit the right file.
    let tmp = unique_tmp("cancel-partial");
    std::fs::create_dir_all(&tmp).unwrap();

    let input_path = tmp.join("file1.mp4");
    std::fs::write(&input_path, b"fake video").unwrap();
    // task.output = file1.mkv (X264 → mkv, no collision
    // with the .mp4 input). Write to that exact path.
    let partial = tmp.join("file1.mkv");
    let partial_check_b = partial.clone();
    std::fs::write(&partial, b"garbage").unwrap();

    let (cmd_tx, evt_rx) = run_test_controller(
        move |task: &EncodeTask,
              _ctx: &mut BatchContext,
              cancel: &CancelToken,
              on_progress: &mut dyn FnMut(FfmpegProgress)| {
            let output = task.output.to_string_lossy().into_owned();
            let argv = crate::test_support::fake_ffmpeg_argv(&task.output);
            assert_eq!(
                argv.last().unwrap(),
                &output,
                "argv.last() must equal task.output for cleanup to work"
            );
            let cancel_signal = cancel.clone();
            let mut progress = move |p: FfmpegProgress| {
                cancel_signal.cancel();
                on_progress(p);
            };
            let res = crate::pipeline::batch::run_ffmpeg_with_progress(
                &argv,
                cancel,
                Some(60.0),
                &mut progress,
            );
            match res {
                Ok(()) => panic!("expected cancel err"),
                Err((category, msg, stderr)) => Ok(EncodingRecord {
                    source_path: task.input.to_string(),
                    source_size_bytes: 0,
                    source_codec: None,
                    source_container: None,
                    source_width: None,
                    source_height: None,
                    source_fps: None,
                    source_duration_secs: None,
                    source_bit_rate_bps: None,
                    started_at: chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
                    ended_at: Some(
                        chrono::DateTime::<Utc>::from_timestamp(1_700_000_010, 0).unwrap(),
                    ),
                    encoding_duration_secs: Some(10.0),
                    encoder_selected: None,
                    encoder_settings: EncoderSettings::default(),
                    ffmpeg_argv: argv.clone(),
                    ffmpeg_stderr_tail: stderr,
                    output_path: None,
                    output_size_bytes: None,
                    output_duration_secs: None,
                    output_codec: None,
                    compression_ratio: None,
                    outcome: EncodingOutcome::Failed,
                    error: Some(crate::types::ErrorContext {
                        category,
                        message: msg,
                        ffmpeg_stderr_tail: String::new(),
                    }),
                }),
            }
        },
        None,
    );

    let files: Vec<crate::types::InputPath> = vec![input(input_path.to_str().unwrap())];
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();
    // The command channel stays open: closing it tells the actor
    // "nobody left to serve", and it exits. The actor is the
    // publisher (design doc §3.10.6 step 5), so an actor that
    // exited would never publish the startup capability and the
    // batch would wait for a state that never arrives.

    let _events = batch_events(batch_events_until_end(&evt_rx));

    assert!(
        !partial_check_b.exists(),
        "partial output {} still on disk after cancel; cleanup did NOT run in the full actor flow",
        partial_check_b.display()
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn the_batch_gets_the_published_capability() {
    // The point of the generation work (design doc §3.10.6
    // step 5): the GUI's batches must see the machine's NVENC
    // capability. The actor publishes it; the batch freezes that
    // publication.
    //
    // Before this, the actor's authoritative copy was never
    // assigned -- the batches ran with an empty capability, so
    // `any_usable()` was false and the selection always fell
    // back to software encoding.
    let capability = Capability::from_support(vec![crate::types::NvencSupport {
        encoder: "h264_nvenc".into(),
        usable: true,
        reason: None,
    }]);
    let saw_usable = Arc::new(AtomicUsize::new(0));
    let saw_usable_clone = saw_usable.clone();
    let (cmd_tx, evt_rx) = run_test_controller_with_capability(
        move |task: &EncodeTask,
              ctx: &mut BatchContext,
              _: &CancelToken,
              _: &mut dyn FnMut(FfmpegProgress)| {
            if ctx.capability().any_usable() {
                saw_usable_clone.fetch_add(1, AtomicOrdering::SeqCst);
            }
            Ok(fake_record(&task.input))
        },
        None,
        Some(capability),
    );
    let files = batch_files("published_capability", 1);
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files,
            },
        ))
        .unwrap();

    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BatchCompleted { .. })),
        "the batch must run: {events:?}"
    );
    assert_eq!(
        saw_usable.load(AtomicOrdering::SeqCst),
        1,
        "the batch must see the machine's capability; events: {events:?}"
    );
}

#[test]
fn a_failed_check_keeps_the_current_capability() {
    // Design doc §3.10.6 step 5: a failed detection keeps the
    // current snapshot. Publishing an empty capability instead
    // would silently turn NVENC off for the next batch -- the
    // machine is fine, only the probe failed.
    let capability = Capability::from_support(vec![crate::types::NvencSupport {
        encoder: "h264_nvenc".into(),
        usable: true,
        reason: None,
    }]);
    let saw_usable = Arc::new(AtomicUsize::new(0));
    let saw_usable_clone = saw_usable.clone();
    // The probe fails -- but only from the **second** probe on: the
    // first one is the startup, which must succeed (the scenario
    // is "the machine is fine, the check failed").
    let probes = Arc::new(AtomicUsize::new(0));
    let probes_for_runner = probes.clone();
    let scripted = scripted_probe_runner(Some(&capability));
    let (cmd_tx, evt_rx) = run_test_controller_with_runner(
        move |task: &EncodeTask,
              ctx: &mut BatchContext,
              _: &CancelToken,
              _: &mut dyn FnMut(FfmpegProgress)| {
            if ctx.capability().any_usable() {
                saw_usable_clone.fetch_add(1, AtomicOrdering::SeqCst);
            }
            Ok(fake_record(&task.input))
        },
        None,
        Some(capability),
        Some(Arc::new(move |args: &[&str]| {
            if args.contains(&"-encoders")
                && probes_for_runner.fetch_add(1, AtomicOrdering::SeqCst) > 0
            {
                return Err(EncodeError::Probe("probe failed".into()));
            }
            scripted(args)
        })),
    );

    // The startup's publication comes first: the runner counts
    // the probes, so the test has to know which probe is which
    // before it sends the check.
    let startup = next_capability_event(&evt_rx);
    assert_eq!(startup.generation(), 1, "the startup publishes first");

    // The user's check (the record is ignored): it fails.
    cmd_tx
        .send(Request::new(
            1,
            Command::CheckCapability {
                ignore_record: true,
            },
        ))
        .unwrap();
    let failure = wait_for_event(&evt_rx, |e| matches!(e, Event::OperationFailed { .. }));
    assert!(
        matches!(failure, Event::OperationFailed { .. }),
        "the failed probe must be reported: {failure:?}"
    );

    // The next batch still sees the machine's capability.
    cmd_tx
        .send(Request::new(
            2,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("failed_probe", 1),
            },
        ))
        .unwrap();
    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BatchCompleted { .. })),
        "the batch must run: {events:?}"
    );
    assert_eq!(
        saw_usable.load(AtomicOrdering::SeqCst),
        1,
        "the failed probe must not have wiped the capability"
    );
}

#[test]
fn a_check_publishes_a_new_generation() {
    // A user-requested check publishes a new capability -- a new
    // generation -- and the next batch freezes that one (design
    // doc §3.10.6 step 5).
    let generations = Arc::new(Mutex::new(Vec::<u64>::new()));
    let generations_clone = Arc::clone(&generations);
    let (cmd_tx, evt_rx) = run_test_controller_with_capability(
        move |task: &EncodeTask,
              ctx: &mut BatchContext,
              _: &CancelToken,
              _: &mut dyn FnMut(FfmpegProgress)| {
            generations_clone.lock().push(ctx.capability().generation());
            Ok(fake_record(&task.input))
        },
        None,
        None,
    );

    // The first batch: the startup publication.
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("generation_first", 1),
            },
        ))
        .unwrap();
    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BatchCompleted { .. })),
        "the first batch must run: {events:?}"
    );

    // The user's check.
    cmd_tx
        .send(Request::new(
            2,
            Command::CheckCapability {
                ignore_record: true,
            },
        ))
        .unwrap();
    let event = wait_for_event(&evt_rx, |e| matches!(e, Event::NvencCapability(_)));
    let events = [event];
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::NvencCapability(_))),
        "the check must publish a capability: {events:?}"
    );

    // The second batch: the new generation.
    cmd_tx
        .send(Request::new(
            3,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("generation_second", 1),
            },
        ))
        .unwrap();
    let events = batch_events(batch_events_until_end(&evt_rx));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BatchCompleted { .. })),
        "the second batch must run: {events:?}"
    );

    let seen = generations.lock().clone();
    assert_eq!(seen.len(), 2, "two batches: {seen:?}");
    assert_eq!(seen[0], 1, "the first batch froze the startup generation");
    assert!(
        seen[1] > seen[0],
        "the check must publish a newer generation: {seen:?}"
    );
}

#[test]
fn an_older_probe_result_does_not_overwrite_a_newer_one() {
    // Design doc §3.10.6 step 5: the probe requests carry a
    // sequence number, and only the newest result is published.
    // An older probe that finishes late must not overwrite the
    // newer snapshot (or the newer record on disk).
    //
    // The first probe blocks in the runner; the second one runs
    // through and publishes; then the first one is let go -- and
    // its result must be dropped.
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_for_runner = Arc::clone(&release);
    // The runner announces its first call. **That is the anchor
    // the test needs**: which probe blocks depends on the thread
    // start order, which the system does not promise. Waiting
    // for the announcement -- before sending the second check --
    // makes "the first probe is the older one" a fact instead of
    // an accident.
    // The fixture blocks the probe that comes **after** the test
    // arms the flag -- not "the n-th call": which thread reaches
    // the runner first is a race (the system does not promise an
    // order), so a count would block the wrong probe.
    let block_next = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let block_next_for_runner = Arc::clone(&block_next);
    // The announcement: the blocked probe says so. The test waits
    // for it **before** it sends the second check -- so "the first
    // probe is the older one" is a fact, not an accident.
    let (blocked_tx, blocked_rx) = unbounded::<()>();
    let runner: crate::job::DetectRunner = Arc::new(move |args: &[&str]| {
        if args.contains(&"-encoders") && block_next_for_runner.swap(false, AtomicOrdering::SeqCst)
        {
            // This probe waits until the test lets it through --
            // it is the probe whose result must arrive late.
            let _ = blocked_tx.send(());
            while !release_for_runner.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        Ok(String::new())
    });
    let (cmd_tx, evt_rx) = run_test_controller_with_runner(
        |task: &EncodeTask,
         _: &mut BatchContext,
         _: &CancelToken,
         _: &mut dyn FnMut(FfmpegProgress)| Ok(fake_record(&task.input)),
        None,
        None,
        Some(runner),
    );

    // The startup's publication comes first: the startup's probe
    // is done, so the next probe to reach the runner is the one
    // the test arms.
    let startup = next_capability_event(&evt_rx);
    assert_eq!(startup.generation(), 1, "the startup publishes first");

    // Arm the fixture: the next probe blocks.
    block_next.store(true, AtomicOrdering::SeqCst);
    // The first check: its probe blocks -- and announces it.
    cmd_tx
        .send(Request::new(
            1,
            Command::CheckCapability {
                ignore_record: true,
            },
        ))
        .unwrap();
    // The announcement: the probe that blocks is the first one --
    // it is the only probe in flight. Now "the older result
    // arrives late" is a fact, not an accident.
    blocked_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the first probe must block");
    // The second check: its probe runs through.
    cmd_tx
        .send(Request::new(
            2,
            Command::CheckCapability {
                ignore_record: true,
            },
        ))
        .unwrap();

    // The second probe's publication: it is the newer one.
    let newer = next_capability_event(&evt_rx);
    assert_eq!(
        newer.generation(),
        2,
        "the newer probe publishes the next generation"
    );

    // Let the first probe finish: its result is the older one.
    release.store(true, std::sync::atomic::Ordering::SeqCst);

    // It must not publish: the sequence check drops it.
    assert_no_event(
        &evt_rx,
        |e| matches!(e, Event::NvencCapability(_)),
        std::time::Duration::from_secs(2),
    );
    // (the assertion is inside `assert_no_event`)
}

#[test]
fn a_verdict_from_an_older_generation_does_not_land() {
    // Design doc §3.10.6 step 5: only the current generation's
    // runtime corrections are merged. A batch that ran under an
    // older publication reported against that publication -- its
    // conclusion is about a machine state the current generation
    // has already replaced.
    let mut publisher = CapabilityPublisher::new(Arc::new(Mutex::new(None)));
    // The publisher starts at generation 0 (nothing published
    // yet).
    let current = publisher.capability().generation();

    // A report from a newer generation than the current one:
    // dropped.
    publisher.merge_verdicts(vec![(current + 1, "h264_nvenc".to_string(), false)]);
    assert!(
        !publisher.capability().verdicts().is_disabled("h264_nvenc"),
        "a stale report must not disable the encoder"
    );

    // A report from the current generation lands.
    publisher.merge_verdicts(vec![(current, "h264_nvenc".to_string(), false)]);
    assert!(
        publisher.capability().verdicts().is_disabled("h264_nvenc"),
        "the current generation's report must land"
    );
}

#[test]
fn a_late_verdict_from_an_old_batch_does_not_disable_the_new_generation() {
    // The scenario: a batch runs under generation 1 and finds an
    // NVENC encoder unusable; while it is still running, a
    // user-requested check publishes generation 2; the batch's late report
    // must not disable the encoder for the next batch (design doc
    // §3.10.6 step 5).
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_for_mock = Arc::clone(&release);
    let recorded = Arc::new(Mutex::new(Vec::<bool>::new()));
    let recorded_for_mock = Arc::clone(&recorded);
    let (cmd_tx, evt_rx) = run_test_controller(
        move |task: &EncodeTask,
              ctx: &mut BatchContext,
              _: &CancelToken,
              _: &mut dyn FnMut(FfmpegProgress)| {
            // The batch blocks here until the check has
            // published the new generation.
            while !release_for_mock.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // What this batch was admitted with.
            recorded_for_mock
                .lock()
                .push(ctx.capability().verdicts().is_disabled("h264_nvenc"));
            // The runtime correction: the encoder turned out to
            // be unusable.
            ctx.capability_mut().verdicts_mut().set("h264_nvenc", false);
            Ok(fake_record(&task.input))
        },
        None,
    );

    // The first batch: it blocks inside the encoder.
    cmd_tx
        .send(Request::new(
            1,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("late_verdict", 1),
            },
        ))
        .unwrap();
    // Wait until the batch is really inside the encoder.
    loop {
        match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Event::FileStarted { .. }) => break,
            Ok(_) => continue,
            Err(e) => panic!("the batch must start: {e}"),
        }
    }

    // The user's check: it publishes a new generation while the
    // first batch is still running.
    cmd_tx
        .send(Request::new(
            2,
            Command::CheckCapability {
                ignore_record: true,
            },
        ))
        .unwrap();
    loop {
        match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Event::NvencCapability(capability)) => {
                if capability.generation() > 1 {
                    break;
                }
            }
            Ok(_) => continue,
            Err(e) => panic!("no capability event: {e}"),
        }
    }

    // Let the first batch finish: its verdict report is late now.
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    loop {
        match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Event::BatchCompleted { .. }) => break,
            Ok(_) => continue,
            Err(e) => panic!("the first batch must finish: {e}"),
        }
    }

    // The second batch: it must not inherit the old conclusion.
    cmd_tx
        .send(Request::new(
            3,
            Command::StartBatch {
                params: EncodeParams::default(),
                files: batch_files("late_verdict_second", 1),
            },
        ))
        .unwrap();
    loop {
        match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Event::BatchCompleted { .. }) => break,
            Ok(_) => continue,
            Err(e) => panic!("the second batch must finish: {e}"),
        }
    }

    let seen = recorded.lock().clone();
    assert_eq!(seen.len(), 2, "two batches: {seen:?}");
    assert!(
        !seen[1],
        "the new generation must not inherit the old batch's verdict: {seen:?}"
    );
}

#[test]
fn the_capability_reaches_the_ui_at_startup() {
    // The GUI's half of the review cycle: the capability (with the
    // advisory flag) reaches the UI without the user asking for it.
    //
    // Pushing the review date is the lib's job -- the resolve
    // does it, so there is no state machine in the frontend
    // (`nvenc::tests::an_expired_record_is_re_stored_with_a_pushed_review_date`
    // covers that half).
    let dir = unique_tmp("capability_event");
    std::fs::create_dir_all(&dir).expect("the temp dir must be created");

    // The stored record is what makes the review advisory
    // appear: the check reuses it (the record is not ignored) and reports
    // that it is past its review date.
    let record = crate::encoder::nvenc::CapabilityRecord {
        version: crate::encoder::nvenc::RECORD_VERSION,
        ffmpeg_version: PROBE_VERSION.to_string(),
        ffmpeg_fingerprint: crate::encoder::nvenc::fingerprint(PROBE_VERSION, ""),
        probed_at: chrono::Utc::now() - chrono::Duration::days(40),
        next_review_at: chrono::Utc::now() - chrono::Duration::days(10),
        encoders: vec![],
    };
    std::fs::write(
        dir.join("nvenc-capability.json"),
        serde_json::to_string(&record).expect("the record must serialize"),
    )
    .expect("the record must be written");

    let (_cmd_tx, evt_rx) = run_test_controller_with_capability(
        make_succeeding_mock().0,
        Some(dir.clone()),
        Some(Capability::from_check(
            Vec::new(),
            Some(chrono::Utc::now() - chrono::Duration::days(40)),
            true,
        )),
    );

    // Wait for the event -- but not for five seconds: the suite runs
    // the tests in parallel, and a loaded machine needs longer than a
    // quiet one. The wait remembers what it saw, so a timeout says
    // *where* the chain stopped instead of just "it did not arrive".
    let mut reported = None;
    let mut seen: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        match evt_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(Event::NvencCapability(capability)) => {
                reported = Some(capability);
                break;
            }
            Ok(other) => {
                if seen.len() < 20 {
                    seen.push(format!("{other:?}"));
                }
            }
            Err(_) => {}
        }
    }
    let capability = reported.unwrap_or_else(|| {
        panic!(
            "the capability never reached the UI within 15s\n\
             --- the events that did arrive ---\n{}\n---",
            seen.join("\n")
        )
    });
    assert!(
        capability.review_due(),
        "the UI needs the advisory flag to show the advisory"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_record_is_reused_unless_the_user_says_the_machine_changed() {
    // "必要时的一次检查"（§3.10.7）：记录成立（新鲜 + 指纹对得上）就
    // **沿用**，不查机器；用户说机器变了才**检查一次**。
    //
    // The runner counts the 1-frame probes (the calls that carry
    // `-c:v`): the fingerprint calls (`-encoders` / `-version`)
    // always happen -- they are how the record's validity is
    // decided -- but a probe is a check of the machine.
    let dir = unique_tmp("record_reuse");
    std::fs::create_dir_all(&dir).expect("the temp dir must be created");

    // A fresh record whose fingerprint matches the fake machine.
    //
    // The machine offers one NVENC encoder: the fake `-encoders`
    // answer lists it, so a check of the machine really runs a
    // 1-frame probe (an empty encoder list would skip the probe
    // entirely -- level 1 of the check, §3.10.5).
    let support = vec![crate::types::NvencSupport {
        encoder: "h264_nvenc".into(),
        usable: true,
        reason: None,
    }];
    let record = crate::encoder::nvenc::CapabilityRecord {
        version: crate::encoder::nvenc::RECORD_VERSION,
        ffmpeg_version: PROBE_VERSION.to_string(),
        ffmpeg_fingerprint: crate::encoder::nvenc::fingerprint(PROBE_VERSION, "h264_nvenc"),
        probed_at: chrono::Utc::now() - chrono::Duration::days(1),
        next_review_at: chrono::Utc::now() + chrono::Duration::days(30),
        encoders: support.clone(),
    };
    std::fs::write(
        dir.join("nvenc-capability.json"),
        serde_json::to_string(&record).expect("the record must serialize"),
    )
    .expect("the record must be written");

    let capability = Capability::from_support(support);
    let scripted = scripted_probe_runner(Some(&capability));
    let probes = Arc::new(AtomicUsize::new(0));
    let probes_for_runner = probes.clone();
    let runner: crate::job::DetectRunner = Arc::new(move |args: &[&str]| {
        if args.contains(&"-c:v") {
            probes_for_runner.fetch_add(1, AtomicOrdering::SeqCst);
        }
        scripted(args)
    });

    let (cmd_tx, evt_rx) = run_test_controller_with_runner(
        make_succeeding_mock().0,
        Some(dir.clone()),
        Some(capability),
        Some(runner),
    );

    // The startup check: the record holds, so the machine is not
    // checked.
    let startup = next_capability_event(&evt_rx);
    assert_eq!(startup.generation(), 1, "the startup publishes first");
    assert_eq!(
        probes.load(AtomicOrdering::SeqCst),
        0,
        "a fresh record must be reused -- no check of the machine"
    );

    // A check that does not ignore the record: still no check of
    // the machine.
    cmd_tx
        .send(Request::new(
            1,
            Command::CheckCapability {
                ignore_record: false,
            },
        ))
        .unwrap();
    let second = next_capability_event(&evt_rx);
    assert_eq!(second.generation(), 2, "the check publishes a generation");
    assert_eq!(
        probes.load(AtomicOrdering::SeqCst),
        0,
        "the record still holds -- it must be reused"
    );

    // A check that ignores the record: the user says the machine
    // changed, so it is checked once.
    cmd_tx
        .send(Request::new(
            2,
            Command::CheckCapability {
                ignore_record: true,
            },
        ))
        .unwrap();
    let third = next_capability_event(&evt_rx);
    assert_eq!(third.generation(), 3, "the check publishes a generation");
    assert!(
        probes.load(AtomicOrdering::SeqCst) > 0,
        "ignoring the record must check the machine"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn encode_error_category_mapping() {
    // Direct test of the mapping helper. Per design §3.8.6.
    let cases = [
        (
            EncodeError::Io(std::io::Error::other("x")),
            ErrorCategory::IoError,
        ),
        (
            EncodeError::Ffmpeg("x".into()),
            ErrorCategory::FfmpegSpawnFailed,
        ),
        (EncodeError::Probe("x".into()), ErrorCategory::ProbeFailed),
        (
            EncodeError::InvalidParam("x".into()),
            ErrorCategory::ConfigError,
        ),
        (
            EncodeError::EncoderNotFound("x".into()),
            ErrorCategory::EncoderSelectionFailed,
        ),
        (
            EncodeError::FpsParse("x".into()),
            ErrorCategory::ConfigError,
        ),
        (EncodeError::UserCancel, ErrorCategory::Cancelled),
    ];
    for (err, expected) in cases {
        assert_eq!(err.category(), expected, "mapping wrong for {err:?}",);
    }
}
