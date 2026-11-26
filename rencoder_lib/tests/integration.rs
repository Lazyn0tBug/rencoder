//! End-to-end integration tests against real `ffmpeg` / `ffprobe`.
//!
//! Each test generates a tiny fixture with `ffmpeg`'s lavfi source, then
//! exercises a public library entry point end-to-end. Tests skip (with a
//! logged warning) when `ffmpeg` / `ffprobe` isn't available so the suite
//! stays CI-friendly without those tools.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use rencoder_lib::{
    data::extract::probe_video_metadata,
    pipeline::batch::{transcode_batch, transcode_single},
    types::{EncodeParams, EncodeTask, EncoderCache, ProcessOutput, Target, VideoFormat},
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// The lib's own log, on the test's output.
///
/// Without a subscriber the lib's `tracing` events go nowhere, so a
/// failure shows the *symptom* ("the partial output must be removed")
/// and not the *cause* (the cleanup's warning, with the I/O error).
/// Design doc §3.11.5: a failure has to carry its evidence.
fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::INFO)
            .try_init();
    });
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fixture_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "rencoder_int_{}_{}_{}",
        label,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_video(dir: &Path, name: &str, fps: u32, duration: u32) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=160x120:d=1",
            "-r",
            &fps.to_string(),
            "-c:v",
            "libx264",
            "-t",
            &duration.to_string(),
        ])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("ffmpeg spawn failed");
    assert!(status.success(), "ffmpeg failed for {name}");
    path
}

fn skip_if_no_ffmpeg() -> bool {
    if !ffmpeg_available() {
        eprintln!("skipping integration test: ffmpeg not available");
        return true;
    }
    false
}

// ---- probe_video_metadata ----

#[test]
fn probe_real_video_returns_codec_and_fps() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = fixture_dir("probe");
    let video = make_video(&dir, "test.mp4", 30, 1);

    let m = probe_video_metadata(&video).expect("probe should succeed");
    assert_eq!(m.codec.as_deref(), Some("h264"));
    let fps = m.best_fps().expect("expected fps");
    assert!((fps - 30.0).abs() < 0.5, "expected ~30 fps, got {fps}");
}

// ---- analyze_records + aggregate_records (U20: analyze_fps deleted) ----

#[test]
fn aggregate_records_groups_mixed_fixtures() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = fixture_dir("analyze_mixed");
    make_video(&dir, "a.mp4", 24, 1);
    make_video(&dir, "b.mp4", 30, 1);
    make_video(&dir, "c.mp4", 60, 1);

    let files = vec![dir.join("a.mp4"), dir.join("b.mp4"), dir.join("c.mp4")];
    let records =
        rencoder_lib::analyzer::records::analyze_records_with_probe(&files, probe_video_metadata);
    let stats = rencoder_lib::analyzer::records::aggregate_records(&records);
    assert_eq!(stats.total_files, 3);
    assert_eq!(stats.parsed_files, 3);
    assert_eq!(stats.failed_files, 0);
    // The Polars cross-tab table has 3 rows: (24, h264), (30, h264),
    // (60, h264).
    assert_eq!(stats.table.height(), 3);
    let table_str = format!("{}", stats.table);
    assert!(table_str.contains("h264"));
}

#[test]
fn aggregate_records_handles_probe_failure_for_missing_file() {
    let dir = fixture_dir("analyze_missing");
    let files = vec![dir.join("does_not_exist.mp4")];
    let records =
        rencoder_lib::analyzer::records::analyze_records_with_probe(&files, probe_video_metadata);
    let stats = rencoder_lib::analyzer::records::aggregate_records(&records);
    assert_eq!(stats.total_files, 1);
    assert_eq!(stats.parsed_files, 0);
    assert_eq!(stats.failed_files, 1);
    assert_eq!(stats.table.height(), 0);
    assert_eq!(stats.failed_paths, vec![dir.join("does_not_exist.mp4")]);
}

// ---- transcode_batch ----

#[test]
fn transcode_batch_end_to_end_produces_output_files() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = fixture_dir("transcode_e2e");
    let input1 = make_video(&dir, "in1.mp4", 30, 1);
    let input2 = make_video(&dir, "in2.mp4", 30, 1);
    let files = vec![
        rencoder_lib::types::InputPath::new(&input1).unwrap(),
        rencoder_lib::types::InputPath::new(&input2).unwrap(),
    ];

    // Build a minimal encoder cache. x264 -> mkv container, so the
    // output filename (in1.mkv) differs from the input (in1.mp4) and
    // doesn't trip the existence-skip check.
    //
    // **x264, not AV1**: the codec is incidental here -- what is
    // under test is the record -> output file path. AV1 (libsvtav1) is
    // an *optional* encoder: the stock ffmpeg on the CI hosts
    // (Ubuntu's apt build, choco's build) does not have it, and the
    // encode then fails with "no AV1 encoder available in this ffmpeg
    // build" (design doc §8 row 79: a missing encoder is a per-file
    // failure, not a panic). x264 is in every ffmpeg build.
    let cache = EncoderCache {
        encoders_output: "V..... libx264              libx264 H.264\n".to_string(),
    };
    let params = EncodeParams {
        format: VideoFormat::X264,
        is_crf: true,
        bitrate_or_crf: 28,
        target: Target::Dir(dir.clone()),
        extensions: vec!["mp4".into()],
        extra_args: vec![],
        max_fps: rencoder_lib::types::MaxFpsConfig::default(),
        ..Default::default()
    };

    let mut ctx = rencoder_lib::pipeline::context::BatchContext::new(
        params,
        std::sync::Arc::new(cache),
        rencoder_lib::encoder::nvenc::Capability::default(),
    );
    let report = transcode_batch(
        &files,
        &mut ctx,
        ProcessOutput::Null,
        &rencoder_lib::pipeline::cancel::CancelToken::new(),
        None,
    );
    let results = report.records;

    assert_eq!(results.len(), 2);
    for record in &results {
        match &record.outcome {
            rencoder_lib::types::EncodingOutcome::Succeeded
            | rencoder_lib::types::EncodingOutcome::ReducedFps => {
                // Output path is derived from input; for AV1 -> mkv,
                // the file should be at <stem>.mkv.
                let stem = std::path::Path::new(&record.source_path)
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap();
                let expected = dir.join(format!("{stem}.mkv"));
                assert!(
                    expected.exists(),
                    "expected output at {}",
                    expected.display()
                );
            }
            other => panic!(
                "expected Succeeded/ReducedFps for {}, got {:?}",
                record.source_path, other
            ),
        }
    }
}

#[test]
fn transcode_batch_reencodes_when_output_exists() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = fixture_dir("skip_exists");
    let input = make_video(&dir, "in.mp4", 30, 1);
    // The output is a DIFFERENT path that already holds a complete
    // video. Re-encoding is the rule (design doc §3.3.4): the config
    // is read once per process, so the previous run's output may have
    // been produced with different settings -- it gets overwritten,
    // never treated as "already done". (Input == output is a different
    // case -- it is rejected outright, see the
    // `fails_when_input_and_output_are_the_same_file` unit test.)
    let output = make_video(&dir, "out.mkv", 30, 1);

    let task = EncodeTask {
        input: rencoder_lib::types::InputPath::new(&input).unwrap(),
        output: output.clone(),
    };
    // The cache must offer the software default for the target
    // format -- the file is re-encoded now, so an encoder has to be
    // selectable.
    let cache = EncoderCache {
        encoders_output: " V..... libx264              libx264 H.264\n".to_string(),
    };
    let params = EncodeParams {
        format: VideoFormat::X264,
        is_crf: true,
        bitrate_or_crf: 28,
        target: Target::File(input.clone()),
        extensions: vec![],
        extra_args: vec![],
        max_fps: rencoder_lib::types::MaxFpsConfig::default(),
        ..Default::default()
    };

    let mut ctx = rencoder_lib::pipeline::context::BatchContext::new(
        params,
        std::sync::Arc::new(cache),
        rencoder_lib::encoder::nvenc::Capability::default(),
    );
    let result = transcode_single(
        &task,
        &mut ctx,
        ProcessOutput::Null,
        &rencoder_lib::pipeline::cancel::CancelToken::new(),
    )
    .expect("transcode_single should succeed");
    // The encode ran and overwrote the existing output.
    assert!(matches!(
        result.outcome,
        rencoder_lib::types::EncodingOutcome::Succeeded
            | rencoder_lib::types::EncodingOutcome::ReducedFps
    ));
    assert!(
        result.output_path.is_some(),
        "the record must name the output that was written"
    );
}

// ---- actor: Command::Cancel aborts a running batch (end-to-end) ----

/// Build a fixture whose codec differs from the target codec.
///
/// The default fixture is h264; encoding it to h264 would hit the
/// same-codec skip rule and no ffmpeg would ever run.
fn make_video_with_codec(dir: &Path, name: &str, codec: &str, duration: u32) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=640x480:r=30",
            "-c:v",
            codec,
            "-t",
            &duration.to_string(),
        ])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("ffmpeg spawn failed");
    assert!(status.success(), "ffmpeg failed for {name}");
    path
}

#[test]
fn actor_cancel_aborts_the_batch_and_removes_the_partial_output() {
    // End-to-end proof of the actor/worker split (§3.10.1.1): the
    // batch runs on a worker thread, so `Command::Cancel` is actually
    // processed while ffmpeg is running. Before the split the command
    // sat in the queue until the batch was over and nothing was
    // aborted.
    if skip_if_no_ffmpeg() {
        return;
    }
    init_tracing();
    let dir = fixture_dir("actor_cancel");
    // Long enough that the encode is still running when the cancel
    // lands (the cancel is sent after the first progress event).
    let input = make_video_with_codec(&dir, "in.mp4", "mpeg4", 120);

    // The real actor, but with the app directory redirected: the
    // capability record, the config and the history must not land in
    // the developer's app directory (design doc §3.10.4).
    let paths = rencoder_lib::config::paths::Paths::resolve(
        dir.join("rencoder.toml"),
        dir.clone(),
        dir.clone(),
        &rencoder_lib::config::app::AppConfig::default(),
    );
    let env = rencoder_lib::job::ActorEnv {
        app_config: std::sync::Arc::new(rencoder_lib::config::app::AppConfig::default()),
        config_path: dir.join("rencoder.toml"),
        paths,
        file_encoder: std::sync::Arc::new(rencoder_lib::pipeline::batch::FfmpegFileEncoder),
        detect_runner: std::sync::Arc::new(rencoder_lib::encoder::nvenc::run_ffmpeg),
    };
    let (cmd_tx, evt_rx) = rencoder_lib::job::start_controller_with(env).into_split();

    let params = EncodeParams {
        format: VideoFormat::X264,
        is_crf: true,
        bitrate_or_crf: 28,
        target: Target::Dir(dir.clone()),
        extensions: vec![],
        extra_args: vec![],
        max_fps: rencoder_lib::types::MaxFpsConfig::default(),
        ..Default::default()
    };
    cmd_tx
        .send(rencoder_lib::job::Request::new(
            1,
            rencoder_lib::job::Command::StartBatch {
                params,
                files: vec![rencoder_lib::types::InputPath::new(&input).unwrap()],
            },
        ))
        .unwrap();

    // Wait for the encode to actually start -- and learn the batch
    // id (the cancel names it, design doc §3.11.3).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut batch_id = None;
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("timed out waiting for the encode to start");
        match evt_rx.recv_timeout(remaining) {
            Ok(rencoder_lib::job::Event::BatchStarted { id, .. }) => batch_id = Some(id),
            Ok(rencoder_lib::job::Event::Progress { .. }) => {
                if batch_id.is_some() {
                    break;
                }
            }
            Ok(rencoder_lib::job::Event::Error { message, .. }) => {
                panic!("encode failed before it started: {message}")
            }
            Ok(_) => continue,
            Err(e) => panic!("no progress event: {e}"),
        }
    }
    let batch_id = batch_id.expect("the batch must announce its id");

    cmd_tx
        .send(rencoder_lib::job::Request::new(
            2,
            rencoder_lib::job::Command::Cancel { batch_id },
        ))
        .unwrap();

    // The batch must end as cancelled -- not as "completed with one
    // failed file".
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut outcome = None;
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match evt_rx.recv_timeout(remaining) {
            Ok(rencoder_lib::job::Event::BatchCancelled {
                completed, total, ..
            }) => {
                outcome = Some((completed, total));
                break;
            }
            Ok(rencoder_lib::job::Event::BatchCompleted { .. }) => {
                panic!("the batch ran to completion — cancel did not abort it")
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let (completed, total) = outcome.expect("expected a BatchCancelled event");
    assert_eq!(total, 1);
    assert_eq!(completed, 0, "no file should have finished");

    // The half-written output must be gone: the cancel branch removes
    // it so the next run doesn't mistake it for a finished encode.
    let output = dir.join("in.mkv");
    assert!(
        !output.exists(),
        "the partial output must be removed on cancel: {}",
        output.display()
    );
}

// The capability flow (which used to have its own cache-init entry
// point) is covered by `tests/nvenc_baseline.rs` -- the check's
// conclusion, the record and the encoder choice.

// ---- c-probe FFI bridge (only with the feature) ----

#[cfg(feature = "c-probe")]
#[test]
fn c_probe_matches_cli_probe_for_real_video() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("cprobe");
    let video = make_video(&dir, "test.mp4", 30, 1);

    let cli = probe_video_metadata(&video).expect("cli probe");
    let ffi = rencoder_lib::ffi::probe_video_metadata_fast(&video)
        .expect("ffi probe should succeed (libavformat compiled)");

    assert_eq!(cli.codec, ffi.codec, "codec should match");
    // fps values can differ slightly in formatting (e.g. "30/1" vs
    // "29.97"); compare the parsed numeric value.
    let cli_fps = cli.best_fps().expect("cli fps");
    let ffi_fps = ffi.best_fps().expect("ffi fps");
    assert!(
        (cli_fps - ffi_fps).abs() < 0.5,
        "fps should be close: cli={cli_fps} ffi={ffi_fps}"
    );
    // Duration: both should be ~1 second.
    assert!(cli.duration_secs.is_some());
    assert!(ffi.duration_secs.is_some());
    let cli_d = cli.duration_secs.unwrap();
    let ffi_d = ffi.duration_secs.unwrap();
    assert!(
        (cli_d - ffi_d).abs() < 0.5,
        "duration should be close: cli={cli_d} ffi={ffi_d}"
    );
}

#[cfg(feature = "c-probe")]
#[test]
fn c_probe_handles_missing_file_gracefully() {
    let missing = std::env::temp_dir().join("rencoder_does_not_exist_xyz.mp4");
    let _ = std::fs::remove_file(&missing);
    // FFI returns a Result -- the C side reports an "open failed" via
    // a zeroed result struct (all fields NULL); the Rust side returns
    // Ok(VideoMetadata::default()) -- the analyzer treats this as a
    // failed probe (no codec / fps).
    let result = rencoder_lib::ffi::probe_video_metadata_fast(&missing)
        .expect("ffi should not panic on missing file");
    assert!(result.codec.is_none());
    assert!(result.best_fps().is_none());
}
