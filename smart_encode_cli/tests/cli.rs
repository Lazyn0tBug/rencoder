//! CLI smoke + end-to-end tests. Spawns the `smart-encode` binary as a
//! subprocess and verifies exit codes, stdout/stderr shape, and file
//! outputs. Tests skip (with a logged warning) when `ffmpeg` is not
//! available so the suite stays CI-friendly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Path to the compiled `smart-encode` binary, supplied by Cargo.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_smart-encode")
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", bin()))
}

fn fixture_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "rencoder_cli_{}_{}_{}",
        label,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_video(dir: &Path, name: &str, fps: u32) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f", "lavfi",
            "-i", "color=c=black:s=160x120:d=1",
            "-r", &fps.to_string(),
            "-c:v", "libx264",
            "-t", "1",
        ])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("ffmpeg spawn failed");
    assert!(status.success(), "ffmpeg failed for {name}");
    path
}

// ---- smoke tests (no ffmpeg required) ----

#[test]
fn version_prints_and_exits_zero() {
    let out = run(&["--version"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("smart-encode"),
        "expected version string, got: {stdout}"
    );
}

#[test]
fn top_level_help_prints_subcommands() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("encode"));
    assert!(stdout.contains("analyze"));
}

#[test]
fn encode_help_prints_options() {
    let out = run(&["encode", "--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for opt in ["--format", "--bitrate", "--max-fps", "--nv"] {
        assert!(stdout.contains(opt), "expected {opt} in encode help");
    }
}

#[test]
fn analyze_help_prints_options() {
    let out = run(&["analyze", "--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for opt in ["--dir", "--parallel", "--verbose"] {
        assert!(stdout.contains(opt), "expected {opt} in analyze help");
    }
}

#[test]
fn unknown_subcommand_exits_nonzero() {
    let out = run(&["frobnicate"]);
    assert!(!out.status.success());
}

// ---- end-to-end (requires ffmpeg) ----

#[test]
fn analyze_subcommand_produces_stats_for_real_videos() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("analyze");
    make_video(&dir, "a.mp4", 24);
    make_video(&dir, "b.mp4", 30);
    make_video(&dir, "c.mp4", 60);

    let out = run(&["analyze", "-d", dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Total: 3 files"), "expected 'Total: 3 files', got: {stdout}");
    assert!(stdout.contains("30 fps"));
    assert!(stdout.contains("60 fps"));
}

#[test]
fn analyze_subcommand_writes_verbose_file() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("analyze_v");
    make_video(&dir, "x.mp4", 30);
    let stats_path = dir.join("stats.txt");

    let out = run(&[
        "analyze",
        "-d",
        dir.to_str().unwrap(),
        "-v",
        "-o",
        stats_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stats_path.exists(), "expected stats file at {}", stats_path.display());
    let content = std::fs::read_to_string(&stats_path).unwrap();
    assert!(content.contains("30 fps"));
    assert!(content.contains("Total:"));
    assert!(content.contains("x.mp4"));
}

#[test]
fn encode_subcommand_produces_output_file() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("encode_e2e");
    let input = make_video(&dir, "movie.mp4", 30);
    let out = run(&[
        "encode",
        "-f",
        input.to_str().unwrap(),
        "-c", "av1",
        "--crf",
        "-b", "45",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected_output = dir.join("movie.mkv");
    assert!(expected_output.exists(), "expected output at {}", expected_output.display());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Summary"), "expected Summary block in stdout, got: {stdout}");
}

#[test]
fn encode_rejects_missing_target() {
    // No --file, no --dir -> should error with a clear message.
    let out = run(&["encode", "-c", "av1"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--file") && stderr.contains("--dir"),
        "expected error mentioning --file and --dir, got: {stderr}"
    );
}
