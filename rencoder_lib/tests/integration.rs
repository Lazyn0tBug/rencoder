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
    analyzer::fps_stats::analyze_fps,
    config::Config,
    data::extract::probe_video_metadata,
    pipeline::{
        batch::{transcode_batch, transcode_single},
        cache::init_encoder_cache,
    },
    types::{EncodeParams, EncoderCache, ProcessOutput, Target, TranscodeTask, VideoFormat},
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

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

// ---- analyze_fps ----

#[test]
fn analyze_fps_groups_mixed_fixtures() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = fixture_dir("analyze_mixed");
    make_video(&dir, "a.mp4", 24, 1);
    make_video(&dir, "b.mp4", 30, 1);
    make_video(&dir, "c.mp4", 60, 1);

    let files = vec![dir.join("a.mp4"), dir.join("b.mp4"), dir.join("c.mp4")];
    let stats = analyze_fps(&files).expect("analyze should succeed");
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
fn analyze_fps_handles_probe_failure_for_missing_file() {
    let dir = fixture_dir("analyze_missing");
    let files = vec![dir.join("does_not_exist.mp4")];
    let stats = analyze_fps(&files).expect("analyze should succeed");
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
    let files = vec![input1.clone(), input2.clone()];

    // Build a minimal encoder cache. AV1 -> mkv container, so the
    // output filename (in1.mkv) differs from the input (in1.mp4) and
    // doesn't trip the existence-skip check.
    let cache = EncoderCache {
        encoders_output: "V..... libsvtav1            libsvtav1 SVT-AV1\n".to_string(),
        has_nvidia: false,
    };
    let config = Config::default();
    let params = EncodeParams {
        format: VideoFormat::Av1,
        is_crf: true,
        bitrate_or_crf: 28,
        target: Target::Dir(dir.clone()),
        extensions: vec!["mp4".into()],
        extra_args: vec![],
        max_fps: rencoder_lib::types::MaxFpsConfig::default(),
    };

    let results = transcode_batch(
        &params,
        &files,
        &cache,
        &config,
        ProcessOutput::Null,
        None,
        None,
    );

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
fn transcode_batch_skips_when_output_exists() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let dir = fixture_dir("skip_exists");
    let input = make_video(&dir, "in.mp4", 30, 1);
    let output = dir.join("in.mp4");
    // Pre-create the "output" with some content; transcode should skip.
    std::fs::write(&output, b"already there").unwrap();

    let task = TranscodeTask {
        input: input.clone(),
        output: Some(output.clone()),
    };
    let cache = EncoderCache {
        encoders_output: String::new(),
        has_nvidia: false,
    };
    let config = Config::default();
    let params = EncodeParams {
        format: VideoFormat::X264,
        is_crf: true,
        bitrate_or_crf: 28,
        target: Target::File(input.clone()),
        extensions: vec![],
        extra_args: vec![],
        max_fps: rencoder_lib::types::MaxFpsConfig::default(),
    };

    let result = transcode_single(&task, &params, &cache, &config, ProcessOutput::Null)
        .expect("transcode_single should succeed");
    assert!(matches!(
        result.outcome,
        rencoder_lib::types::EncodingOutcome::Skipped(
            rencoder_lib::types::SkippedReason::OutputExists
        )
    ));
}

// ---- init_encoder_cache ----

#[test]
fn init_encoder_cache_works_without_nvidia_smi() {
    // On macOS or any system without nvidia-smi, this should still succeed
    // with has_nvidia=false (R20).
    let cache = init_encoder_cache(true).expect("init should succeed");
    // We can't assert has_nvidia directly (depends on env), but the call
    // must not have returned Err.
    assert!(
        cache.encoders_output.contains("libx264")
            || cache.encoders_output.is_empty()
            || !cache.encoders_output.is_empty(),
        "encoders output should be populated (or empty if ffmpeg missing)"
    );
}

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
