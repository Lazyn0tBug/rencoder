//! CLI smoke + end-to-end tests. Spawns the `rencoder` binary as a
//! subprocess and verifies exit codes, stdout/stderr shape, and file
//! outputs. Tests skip (with a logged warning) when `ffmpeg` is not
//! available so the suite stays CI-friendly.
//!
//! U17 split: the CLI keeps only `target` (`-f`/`-d`) and `action`
//! (`encode`/`analyze`) plus per-run overrides (`-i`, `--config`, `-o`,
//! passthrough args). Everything else lives in a TOML config file that
//! tests pass via `--config`. `write_config()` is the helper that
//! materializes that file for the integration tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Path to the compiled `rencoder` binary, supplied by Cargo.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rencoder")
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

/// A per-process scratch directory for the spawned binary.
///
/// The binary keeps its config, its history, its logs and its NVENC
/// capability record in its app directory. In a test that must not be
/// the build directory: the child gets env vars pointing at a scratch
/// directory instead -- they exist for exactly this ("explicit
/// override for debugging / CI").
fn scratch_dir() -> PathBuf {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("rencoder_cli_env_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch dir must be created");
        dir
    })
    .clone()
}

fn run(args: &[&str]) -> std::process::Output {
    let scratch = scratch_dir();
    Command::new(bin())
        .args(args)
        .env("RENENCODER_CONFIG", scratch.join("rencoder.toml"))
        .env("RENENCODER_DATA_DIR", scratch.join("data"))
        .env("RENENCODER_LOG_DIR", scratch.join("log"))
        .env(
            "RENENCODER_NVENC_CACHE",
            scratch.join("nvenc-capability.json"),
        )
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

/// Write a TOML config file into `dir` and return its path. Tests use
/// this to express what would previously have been CLI flags; the
/// `--config` flag wires it into the run.
fn write_config(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("rencoder.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

fn make_video(dir: &Path, name: &str, fps: u32) -> PathBuf {
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
            "1",
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("rencoder"),
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
    // U17 CLI surface: target (-f/-d), interactive, config path,
    // output path, passthrough. Format / bitrate / max-fps / crf /
    // ext have moved to the config file. `nv` is the deliberate
    // exception -- the CLI can override it per run.
    let out = run(&["encode", "--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for opt in ["--file", "--dir", "--config", "--output", "--nv", "--no-nv"] {
        assert!(stdout.contains(opt), "expected {opt} in encode help");
    }
    for removed in ["--format", "--bitrate", "--max-fps", "--crf", "--ext"] {
        assert!(
            !stdout.contains(removed),
            "{removed} should have moved to config"
        );
    }
}

#[test]
fn detect_subcommand_prints_a_report() {
    // Writes the capability record next to the binary (the app dir);
    // that is the point of the command.
    let out = run(&["detect"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("NVENC capability"), "got: {stdout}");
    assert!(stdout.contains("ffmpeg"), "got: {stdout}");
}

#[test]
fn detect_flag_appears_in_the_help() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--detect"), "expected --detect in help");
    assert!(stdout.contains("detect"), "expected the detect subcommand");
}

#[test]
fn analyze_help_prints_options() {
    let out = run(&["analyze", "--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for opt in ["--dir", "--file", "--config", "--output"] {
        assert!(stdout.contains(opt), "expected {opt} in analyze help");
    }
    for removed in ["--format", "--parallel", "--ext"] {
        assert!(
            !stdout.contains(removed),
            "{removed} should have moved to config"
        );
    }
}

#[test]
fn unknown_subcommand_exits_nonzero() {
    let out = run(&["frobnicate"]);
    assert!(!out.status.success());
}

// ---- end-to-end (requires ffmpeg) ----

#[test]
fn analyze_subcommand_produces_csv_for_real_videos() {
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
    // Default output is per-file CSV to stdout.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("path,fps,codec,duration_secs,file_bytes\n"));
    assert!(stdout.contains("/a.mp4,24,"));
    assert!(stdout.contains("/b.mp4,30,"));
    assert!(stdout.contains("/c.mp4,60,"));
    assert_eq!(
        stdout.lines().count(),
        4,
        "expected header + 3 rows, got: {stdout}"
    );
    // Summary on stderr.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Analyzed 3 file(s) (3 parsed, 0 failed)"));
}

#[test]
fn analyze_subcommand_writes_csv_to_file() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("analyze_csv");
    make_video(&dir, "x.mp4", 30);
    let stats_path = dir.join("stats.csv");

    let out = run(&[
        "analyze",
        "-d",
        dir.to_str().unwrap(),
        "-o",
        stats_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stats_path.exists(),
        "expected CSV at {}",
        stats_path.display()
    );
    let content = std::fs::read_to_string(&stats_path).unwrap();
    assert!(content.starts_with("path,fps,codec,duration_secs,file_bytes\n"));
    assert!(content.contains("/x.mp4,30,"));
}

#[test]
fn analyze_text_format_writes_to_file() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("analyze_text");
    make_video(&dir, "x.mp4", 30);
    let stats_path = dir.join("stats.txt");
    let cfg = write_config(&dir, "[analyze]\nformat = \"text\"\n");

    let out = run(&[
        "analyze",
        "-d",
        dir.to_str().unwrap(),
        "--config",
        cfg.to_str().unwrap(),
        "-o",
        stats_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stats_path.exists(),
        "expected text file at {}",
        stats_path.display()
    );
    let content = std::fs::read_to_string(&stats_path).unwrap();
    assert!(
        content.contains("fps"),
        "expected Polars table with fps column"
    );
    assert!(content.contains("30"), "expected 30 fps in table");
    assert!(content.contains("h264"), "expected codec in table");
    assert!(content.contains("Total:"));
}

#[test]
fn analyze_table_format_emits_framed_per_file_table() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("analyze_table");
    make_video(&dir, "a.mp4", 24);
    make_video(&dir, "b.mp4", 30);
    let cfg = write_config(&dir, "[analyze]\nformat = \"table\"\n");

    let out = run(&[
        "analyze",
        "-d",
        dir.to_str().unwrap(),
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("┌") && stdout.contains("└") && stdout.contains("│"),
        "expected framed table borders, got: {stdout}"
    );
    let row_separator_count = stdout.matches("│").count();
    assert!(
        row_separator_count >= 6,
        "expected at least 6 '|' borders (header + 2 rows + cols), got {row_separator_count} in: {stdout}"
    );
    assert!(stdout.contains("path") && stdout.contains("fps") && stdout.contains("codec"));
}

#[test]
fn encode_subcommand_produces_output_file() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("encode_e2e");
    let input = make_video(&dir, "movie.mp4", 30);
    // U17: format / crf / bitrate live in config now.
    let cfg = write_config(
        &dir,
        "[encode]\nformat = \"av1\"\ncrf = 45\nbitrate = 5000\n",
    );

    let out = run(&[
        "encode",
        "-f",
        input.to_str().unwrap(),
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected_output = dir.join("movie.mkv");
    assert!(
        expected_output.exists(),
        "expected output at {}",
        expected_output.display()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Summary"),
        "expected Summary block in stdout, got: {stdout}"
    );
}

#[test]
fn encode_rejects_missing_target() {
    // No --file, no --dir -> should error with a clear message.
    let out = run(&["encode"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--file") && stderr.contains("--dir"),
        "expected error mentioning --file and --dir, got: {stderr}"
    );
}

// ---- interrupt: the records survive it (design doc §3.11.2) ----

/// Spawn the binary with the scratch environment, so the caller can
/// signal it. The output is discarded: ffmpeg writes a lot of it, and
/// a pipe nobody reads would fill up and block the child.
#[cfg(unix)]
fn spawn(args: &[&str], logs: Option<(&Path, &Path)>) -> std::process::Child {
    let scratch = scratch_dir();
    let mut cmd = Command::new(bin());
    cmd.args(args)
        .env("RENENCODER_CONFIG", scratch.join("rencoder.toml"))
        .env("RENENCODER_DATA_DIR", scratch.join("data"))
        .env("RENENCODER_LOG_DIR", scratch.join("log"))
        .env(
            "RENENCODER_NVENC_CACHE",
            scratch.join("nvenc-capability.json"),
        );
    match logs {
        Some((out, err)) => {
            cmd.stdout(std::fs::File::create(out).expect("the log file must be creatable"))
                .stderr(std::fs::File::create(err).expect("the log file must be creatable"));
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    cmd.spawn().expect("the binary must start")
}

/// Make a video of `seconds` at 1080p.
///
/// The resolution matters: the encode has to be slow enough that the
/// signal lands *during* it, on any machine.
#[cfg(unix)]
fn make_video_of_seconds(dir: &Path, name: &str, seconds: u32) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=black:s=1920x1080:d={seconds}"),
            "-r",
            "30",
            "-c:v",
            "libx264",
            "-t",
            &seconds.to_string(),
        ])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("ffmpeg spawn failed");
    assert!(status.success(), "ffmpeg failed to make {name}");
    path
}

/// Wait until `marker` shows up in `file` (or give up).
#[cfg(unix)]
fn wait_for(file: &Path, marker: &str, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        if text.contains(marker) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("{marker} never showed up in {}", file.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// An interrupt must not cost the run: the files that were already
/// encoded stay in the records, and the process says it was
/// interrupted (130 = 128 + SIGINT).
///
/// Before the cancel contract, the signal handler removed the partial
/// output and exited right there -- the finished files were never
/// written anywhere.
#[cfg(unix)]
#[test]
fn an_interrupt_keeps_the_records_of_the_files_that_finished() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = fixture_dir("interrupt");
    // The inputs live in their own directory: the config and the
    // output file would otherwise be collected as inputs too.
    let inputs = dir.join("in");
    std::fs::create_dir_all(&inputs).unwrap();
    // The first file is short (it finishes before the signal lands);
    // the second is long (the signal lands while it is encoding).
    make_video_of_seconds(&inputs, "a-short.mp4", 1);
    make_video_of_seconds(&inputs, "b-long.mp4", 60);
    // AV1 is slow enough that the encode is still running when the
    // signal arrives.
    let config = write_config(&dir, "[encode]\nformat = \"av1\"\ncrf = 40\n");
    let out = dir.join("records.parquet");

    let out_log = dir.join("stdout.log");
    let err_log = dir.join("stderr.log");
    let mut child = spawn(
        &[
            "encode",
            "-d",
            inputs.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
        Some((&out_log, &err_log)),
    );

    // Wait until the second file is really being encoded: the first
    // one is done by then, and the batch is busy (so the signal has
    // something to interrupt). A fixed sleep would be a race -- the
    // signal can land before the CLI even installed its handler, and
    // then the default disposition kills the process.
    wait_for(&err_log, "b-long.mp4", std::time::Duration::from_secs(120));

    let status = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .expect("kill must run");
    assert!(status.success(), "the signal must be delivered");

    let status = child.wait().expect("the child must exit");
    assert_eq!(
        status.code(),
        Some(130),
        "an interrupted run exits with 130 (128 + SIGINT); got {status:?} signal={:?}",
        std::os::unix::process::ExitStatusExt::signal(&status),
    );

    // The records of the finished file are on disk.
    assert!(
        out.exists(),
        "the records must be written before the process exits: {}",
        out.display()
    );
    let records =
        rencoder_lib::data::store::read_records(&out).expect("the records must be readable");
    assert!(
        !records.is_empty(),
        "the file that finished before the signal must be in the records"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
