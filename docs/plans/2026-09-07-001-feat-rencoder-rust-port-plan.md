---
title: "Rencoder: Rust port of encode.zsh and video_fps_stats.zsh"
status: active
date: 2026-09-07
origin: docs/smart_code.md
deepened: 2026-09-07
---

# Rencoder: Rust port of `encode.zsh` and `video_fps_stats.zsh`

## Summary

Rencoder is a Rust workspace that ports two shell scripts (`encode.zsh` v10.0.3 for transcode, `video_fps_stats.zsh` v2.3 for FPS analysis) into a layered CLI + library crate pair. The library owns all business logic (probing, encoder selection, command building, transcoding, analysis); the CLI owns clap arg parsing, dialoguer interaction, and ctrlc signal handling. The shell-script behavior is the source of truth; the rewrite keeps the same external surface (`ffmpeg`, `ffprobe`, optional `nvidia-smi`) but replaces `jq` / `fd` / `xargs` / `grep` / `awk` / `nproc` with pure-Rust crates (`serde_json`, `walkdir`, `rayon`, string ops, `num_cpus`).

## Problem Frame

The scripts work but accumulate friction: zero compile-time checks, two-step manual config (a JSON file plus a shell wrapper), scattered `jq` parsing in heredocs, `fd` for file collection that misbehaves on symlinks, and `xargs -P` for parallelism that obscures errors. The Rust port keeps the user-visible behavior identical (the integration tests compare outputs against the scripts) while replacing the shell-pipeline glue with typed APIs.

Two design decisions drive the rewrite:

1. **`MaxFpsConfig` as a pure predicate** (`need_reduce`), with `-r <fps>` parameter generation owned by `build_ffmpeg_command`. Single responsibility lets the predicate be tested without invoking ffmpeg.
2. **`--nv` plus `nvidia-smi`** for hardware acceleration, with silent software-encoding fallback. The script's `ffmpeg -encoders` substring match is NOT trusted as a GPU proxy; it can report a compiled-in encoder with no GPU present.

The plan is **Deep** because it crosses 17 phases in the origin doc, has cross-cutting concerns (workspace structure, layered errors, two frontends eventually), and ships optional features (`c-probe` C FFI bridge, GUI prototype) that need their own sequencing.

## Requirements

Traced from `docs/smart_code.md` §8 (21 confirmation items). All confirmed by the user.

| ID  | Requirement |
|-----|-------------|
| R1  | External deps are `ffmpeg`, `ffprobe`, and (only when `--nv` is passed) `nvidia-smi`. No `jq`, `fd`, `xargs`, `grep`, `cut`, `awk`, `nproc`. |
| R2  | `ffmpeg-next` is rejected; `c-probe` is an optional C FFI feature instead. |
| R3  | `MaxFpsConfig` exposes a predicate only; parameter mapping lives in `build_ffmpeg_command`. |
| R4  | `-r <fps>` is an FFmpeg output option: must appear after `-i`, before the output path. |
| R5  | `should_skip_encoding` follows the 3-row decision table in `docs/smart_code.md` §3.3. |
| R6  | `need_reduce` is a method on `MaxFpsConfig`; no free function. |
| R7  | `EncoderCache` does not derive `Clone`; passed by `&`. |
| R8  | `TranscodeResult::Failed(EncodeError)` carries the structured error. |
| R9  | `probe_video_metadata_auto` returns `Result<VideoMetadata, EncodeError>`. |
| R10 | `collect_files` uses `walkdir`, does not follow symlinks, does not depend on `fd`. |
| R11 | Parallel analysis uses `rayon`; no `xargs`. |
| R12 | NVIDIA detection is `--nv` AND `nvidia-smi -L` reporting a GPU. No `ffmpeg -encoders` inference. |
| R13 | `ProcessOutput::Inherit` is default; `Piped` for the future GUI (kept; §3.8 GUI currently bypasses via `cmd.spawn()`). |
| R14 | Signal handling uses `ctrlc` plus `parking_lot::Mutex` (cross-platform incl. Windows console events). |
| R15 | `EncodeError` is `#[non_exhaustive]`. |
| R16 | `FpsStats::new(total_files)` initializes the total count. |
| R17 | The `c-probe` FFI bridge is stateless, re-entrant, and `rayon`-safe. |
| R18 | The analyzer supports both serial and optional parallel modes. |
| R19 | The CLI exposes `encode` and `analyze` subcommands. |
| R20 | `nvidia-smi` missing or returning no GPUs silently falls back to software encoding; not an `EncodeError`. |
| R21 | `build_ffmpeg_command` ignores `MaxFpsConfig` entirely when `src_fps = None`; it does not inject `-r`. |

## Key Technical Decisions

KTDs traceable to `docs/smart_code.md` §2.

| ID  | Decision | Why |
|-----|----------|-----|
| K1  | Library/CLI split (`rencoder_lib` + `rencoder_cli`). | Lets a future GUI/TUI/web frontend reuse the library without dragging in clap/dialoguer. |
| K2  | `thiserror` in the library, `anyhow` in the CLI. | Library returns structured, exhaustively-matchable errors; CLI prints-and-exits. Mixing layers would leak UI concerns into the library. |
| K3  | `#[non_exhaustive]` on `EncodeError`. | Forward-compat: Phase 8's `TranscodeResult::Failed(EncodeError)` consumers stay compilable when new variants are added in Phase 16/17. |
| K4  | `walkdir` (no `fd`), `serde_json` (no `jq`), `rayon` (no `xargs`). | Pure-Rust toolchain; no external process spawns for orchestration. |
| K5  | `clap` (derive) for CLI parsing; `dialoguer` for interactive wizard. | Standard Rust CLI ecosystem; minimal boilerplate. |
| K6  | `Command::new("ffmpeg")` with `ProcessOutput::Inherit` for stdout/stderr. | Matches the script's default; future GUI uses `Piped` (R13). |
| K7  | `EncoderCache` is a one-shot struct built at the start of a batch; passed by `&` to every per-file call. | The `ffmpeg -encoders` output never changes during a run; cloning the (potentially multi-MB) string would be wasteful. |
| K8  | `-r <fps>` is placed after `-i`, before the output path, in `build_ffmpeg_command`. | Putting `-r` before `-i` would force the input framerate, causing A/V desync (origin §3.3). |
| K9  | `Config::load(path)` returns `Self::default()` when the path does not exist (not an error). | User-friendly: a missing config file means "use defaults", not "fail to start". |
| K10 | `nvidia-smi` failure is logged at `info!` and falls through to software encoding. | Per R20; matches script behavior where `--nv` is best-effort. |

## High-Level Technical Design

```
                            rencoder_cli (Phase 8-13)
                            ----------------------------
        clap args  --->  dialoguer (interactive)
                \          |
                 \         v
                  +-> signal.rs (ctrlc + parking_lot::Mutex)
                              |
                              v
                    rencoder_lib  (Phase 1-7)
                    --------------------------------
   +-----------+-----------+------------+---------------+-----------+
   |           |           |            |               |           |
 probe      encoder     command     processor      analyzer      config
 (ffprobe)  (cache +    (argv       (single +       (FPS         (JSON
            select)     builder)    batch)         stats)       loader)
   |           |           |            |               |
   v           v           v            v               v
 ffmpeg    ffmpeg -     ffmpeg      ffmpeg           ffmpeg
 ffprobe   encoders     argv        process          ffprobe
           nvidia-smi
```

**Data flow during a transcode batch:**

1. CLI parses clap args into `EncodeParams` + a `--config` path.
2. `Config::load(path)` parses JSON; `cfg.max_fps_config()` produces the runtime `MaxFpsConfig`.
3. `init_encoder_cache(enable_nv)` runs `ffmpeg -encoders` once and (if `--nv`) `nvidia-smi -L`.
4. `ctrlc::set_handler` registers the partial-file cleanup using a `parking_lot::Mutex<Option<PathBuf>>`.
5. `collect_files(target, extensions)` walks `walkdir`, filters by extension, returns `Vec<PathBuf>`.
6. `transcode_batch` iterates: for each file, `transcode_single` runs:
   1. Check output existence (skip if so, except when force-reduce applies).
   2. `probe_video_metadata` (single `ffprobe`) → `VideoMetadata`.
   3. `MaxFpsConfig::need_reduce(src_fps)` predicate.
   4. `should_skip_encoding(codec, format)` against the decision table.
   5. `select_encoder(format, cache, config_path?)` from cache.
   6. `build_ffmpeg_command(task, params, encoder, src_fps)` with `-r` placed correctly.
   7. `Command::new("ffmpeg").args(...).status()` with `ProcessOutput::Inherit`.

**`build_ffmpeg_command` argv layout (must match origin §3.3 example):**

```
ffmpeg -y -i input.mp4 \
  -c:v <encoder> [-preset ...] [-crf ... | -b:v ...] \
  -c:a <audio_encoder> [-b:a ...] \
  -r <MAX_FPS> \   # only when src_fps is Some AND need_reduce is true
  output.mkv
```

The `-r` block is the single most error-prone position; the unit test in U5 must assert argv ordering, not just argument presence.

## Implementation Units

### U1. Foundation: workspace, types, errors, config

- **Goal**: Two-crate workspace with the public type surface and error/config plumbing that later units depend on.
- **Requirements**: R7, R8, R15, R16, R20 (config layer), K1, K2, K3, K9.
- **Dependencies**: none.
- **Files**:
  - `Cargo.toml`
  - `CLAUDE.md`
  - `.gitignore`
  - `rencoder_lib/Cargo.toml`
  - `rencoder_lib/src/lib.rs`
  - `rencoder_lib/src/error.rs`
  - `rencoder_lib/src/types.rs`
  - `rencoder_lib/src/path_util.rs`
  - `rencoder_lib/src/config.rs`
  - `rencoder_lib/src/collect.rs`
  - `rencoder_lib/src/ffmpeg/{mod,probe,encoder,encoder_cache,command}.rs`
  - `rencoder_lib/src/fps/{mod,parse,limit}.rs`
  - `rencoder_lib/src/analyzer/{mod,fps_stats}.rs`
  - `rencoder_lib/src/processor/{mod,batch}.rs`
  - `rencoder_cli/Cargo.toml`
  - `rencoder_cli/src/{main,args,interact,log,signal}.rs`
- **Status**: complete in initial commit (commit `8a35100`). Tests in `rencoder_lib/src/config.rs` cover missing-file, missing-keys, unknown-keys, and round-trip cases (5 passing).
- **Test scenarios**: `cargo check --workspace` is clean; `cargo test -p rencoder_lib --lib config` passes 5/5.

### U2. Max FPS predicate

- **Goal**: Implement `MaxFpsConfig::need_reduce` to encode the encode.zsh semantics.
- **Requirements**: R3, R6, R21.
- **Dependencies**: U1.
- **Files**: `rencoder_lib/src/types.rs`.
- **Approach**: Strict `>` with epsilon on the right side (`src_fps > max_fps + eps`). At the boundary `src_fps == max_fps`, the file is NOT reduced (preserves quality on already-at-cap content). The 29.97-vs-30 case falls into the band `[max_fps, max_fps + eps]` and stays untouched.
- **Test scenarios** (must match origin §6):
  - `need_reduce(60.0)` with `max_fps=30, eps=0.05` → `true`
  - `need_reduce(30.0)` → `false` (boundary)
  - `need_reduce(29.97)` → `false` (within epsilon band)
  - `need_reduce(25.0)` → `false` (below cap)
  - `need_reduce(NaN)` → `false` (defensive; caller preconditions say it won't happen)
  - `MaxFpsConfig::default().enabled()` → `false`
- **Verification**: `cargo test -p rencoder_lib --lib types` passes; manual sanity check at REPL.

### U3. Probing: `probe_video_metadata` + `parse_fps_value`

- **Goal**: A single `ffprobe` call returns `codec` plus `avg_frame_rate` and `r_frame_rate` (R9 calls this "one probe, many fields"). Plus a pure-Rust fps parser that accepts fractions, integers, and decimals.
- **Requirements**: R9, R10 (uses walkdir later but not here).
- **Dependencies**: U1.
- **Files**:
  - `rencoder_lib/src/ffmpeg/probe.rs`
  - `rencoder_lib/src/fps/parse.rs`
- **Approach**: `Command::new("ffprobe").args(["-v","error","-select_streams","v:0","-show_entries","stream=codec_name,avg_frame_rate,r_frame_rate","-of","json", input])` then `serde_json::from_str`. Map the JSON to `VideoMetadata`. For `parse_fps_value`, split on `/` first; if both sides parse as integers, divide; otherwise try `f64::from_str`. Reject negative, zero, NaN, infinity.
- **Test scenarios**:
  - `parse_fps_value("30000/1001")` → `29.97002997002997` within `1e-6`
  - `parse_fps_value("25")` → `25.0`
  - `parse_fps_value("29.97")` → `29.97`
  - `parse_fps_value("0/1")` → `EncodeError::FpsParse`
  - `parse_fps_value("abc")` → `EncodeError::FpsParse`
  - `parse_video_metadata` against a real fixture video (integration test, integration-tests crate).
- **Verification**: All parse cases pass unit tests; probe tests need a small sample video (commit a tiny fixture or skip if absent).

### U4. Encoder cache + selection (incl. NVIDIA)

- **Goal**: One-shot `EncoderCache` builder plus a `select_encoder` that maps a `VideoFormat` plus optional config to a concrete encoder name. `nvidia-smi` is consulted only when `enable_nv` is true (R1, R12, R20).
- **Requirements**: R1, R7, R12, R20.
- **Dependencies**: U1.
- **Files**:
  - `rencoder_lib/src/ffmpeg/encoder_cache.rs`
  - `rencoder_lib/src/ffmpeg/encoder.rs`
- **Approach**: `init_encoder_cache(enable_nv)` runs `ffmpeg -encoders` once, stores output. When `enable_nv`, runs `nvidia-smi -L`; `io::Error` (missing binary) or empty stdout is treated as "no GPU" and logged at `info!`, not as `EncodeError`. `select_encoder` consults the cache via `has_encoder(name)`; the format-to-encoder mapping is hardcoded (libsvtav1 for AV1, libx264 for H.264, libx265 for HEVC, libvpx-vp9 for VP9) with config-file overrides for power users.
- **Test scenarios**:
  - `EncoderCache::has_encoder("libsvtav1")` against mocked `encoders_output` containing the string → `true`
  - `EncoderCache::has_encoder("libdav1d")` against the same → `false`
  - `init_encoder_cache(false)` when `nvidia-smi` is missing → `Ok(...)` with `has_nvidia=false`, no error
  - `init_encoder_cache(true)` when `nvidia-smi` returns empty → `Ok(...)` with `has_nvidia=false`
  - `init_encoder_cache(true)` when `nvidia-smi` returns one GPU → `Ok(...)` with `has_nvidia=true`
  - Encoder-cache test: verify `ffmpeg -encoders` runs at most once across N calls (mock with `Command` wrapping or test-only seam).
- **Verification**: Unit tests using a mockable subprocess seam; manual smoke test on macOS (where `nvidia-smi` is absent) confirms the silent-fallback path.

### U5. Command builder + single-file transcode

- **Goal**: `build_ffmpeg_command` produces argv matching origin §3.3 example; `transcode_single` runs the full pipeline.
- **Requirements**: R3, R4, R5, R13, R21.
- **Dependencies**: U2, U3, U4.
- **Files**:
  - `rencoder_lib/src/ffmpeg/command.rs`
  - `rencoder_lib/src/processor/batch.rs`
- **Approach**: `build_ffmpeg_command(task, params, encoder, src_fps)` returns `Vec<String>`. The `-r` block is inserted only when `params.max_fps.enabled()` AND `src_fps.is_some()` AND `params.max_fps.need_reduce(src_fps.unwrap())` — note the R21 carve-out for `src_fps = None`. Audio codec selection mirrors the script (`-c:a copy` if compatible, else `libopus`/`aac`). `transcode_single` is the orchestrator: existence check → probe → predicate → skip-table → encoder selection → command build → `Command::status()`.
- **Test scenarios**:
  - `build_ffmpeg_command` with `max_fps=30, src_fps=Some(60.0)` produces argv with `-r 30` placed AFTER `-i` and BEFORE the output path. Assert position by index, not just presence.
  - Same with `src_fps=None` produces no `-r` (R21).
  - `build_ffmpeg_command` with `max_fps=0` produces no `-r` regardless.
  - `should_skip_encoding("h264", VideoFormat::X264)` → `true`.
  - `should_skip_encoding("h264", VideoFormat::X264)` when `need_reduce` is `true` → `false` (ReducedFps path).
  - `should_skip_encoding("vp9", VideoFormat::Av1)` → `false`.
  - `transcode_single` against a non-existent file → `TranscodeResult::Failed(EncodeError::Io(_))`.
  - `transcode_single` against an existing output file → `TranscodeResult::SkippedExists`.
- **Verification**: All argv-ordering assertions pass; manual end-to-end with a 5-second sample video confirms `Inherit` output renders.

### U6. File collection + batch transcode

- **Goal**: `collect_files` walks `walkdir` without following symlinks; `transcode_batch` iterates without aborting on failures.
- **Requirements**: R10.
- **Dependencies**: U5.
- **Files**:
  - `rencoder_lib/src/collect.rs`
  - `rencoder_lib/src/processor/batch.rs`
- **Approach**: `walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_map(...)` filters by extension (case-insensitive). `transcode_batch` calls `transcode_single` per file, accumulates `Vec<(PathBuf, TranscodeResult)>`, and never short-circuits on `Failed(_)`. The optional `result_callback` is fired after each file for live UI updates.
- **Test scenarios**:
  - `collect_files` on a directory with `mp4` + `txt` + subdirectory containing `mkv` returns 2 paths.
  - `collect_files` skips a symlink that loops back to the parent (no infinite recursion).
  - `collect_files` with empty `extensions` returns all files.
  - `transcode_batch` over a list where the second file fails returns 3 results, with the failure as `Failed(_)`, the others as `Success`.
  - `transcode_batch` invokes the callback once per file, in order.
- **Verification**: Unit tests with `tempfile`-style fixture directories; smoke test on a small multi-file batch.

### U7. FPS analyzer

- **Goal**: `analyze_fps` produces `FpsStats` matching `video_fps_stats.zsh` output format.
- **Requirements**: R16, R18.
- **Dependencies**: U3, U6.
- **Files**:
  - `rencoder_lib/src/analyzer/fps_stats.rs`
- **Approach**: For each file, `probe_video_metadata_auto` → `best_fps()` → `round` to `u32` → `FpsStats::add(fps, path)`. Failures increment `failed_files` and log at `warn!`. `FpsStats::summary` prints group counts; `FpsStats::detailed` lists files per group.
- **Test scenarios**:
  - Empty input list → `FpsStats { total_files: 0, parsed_files: 0, ... }`.
  - Mixed list with 3 distinct fps values (24, 30, 60) groups correctly under `BTreeMap`.
  - One probe failure increments `failed_files` and does NOT panic the batch.
  - `summary()` and `detailed()` produce non-empty strings containing the group counts.
- **Verification**: Unit tests with a mockable probe seam; smoke test against a directory of fixtures.

### U8. CLI core: args + signal + logging

- **Goal**: `clap`-derived args for `encode` and `analyze` subcommands; `ctrlc`c` handler that cleans up partial output; `env_logger` initialization.
- **Requirements**: R13, R14, R19.
- **Dependencies**: U1.
- **Files**:
  - `rencoder_cli/src/args.rs`
  - `rencoder_cli/src/signal.rs`
  - `rencoder_cli/src/log.rs`
  - `rencoder_cli/src/main.rs`
- **Approach**: `Args` enum with `Encode(EncodeArgs)` and `Analyze(AnalyzeArgs)` subcommands (clap derive). `EncodeArgs` mirrors origin §3.4. `signal::install_handler` takes `Arc<Mutex<Option<PathBuf>>>` (parking_lot, no poisoning); the per-file `transcode_single` clones the Arc, sets the current output before invoking ffmpeg, and clears it after.
- **Test scenarios**:
  - `Args::try_parse_from(["rencoder", "encode", "-f", "in.mp4"])` parses without error.
  - `Args::try_parse_from(["rencoder", "encode", "--"])` followed by arbitrary extra args preserves them in `extra_args`.
  - `signal::install_handler` registers without panicking; manual SIGINT during a run deletes the partial file.
- **Verification**: `clap` unit tests pass; manual SIGINT test on a long-running encode.

### U9. Interactive mode

- **Goal**: `dialoguer` wizard that prompts for format, CRF/bitrate, target, extensions, max-fps, NVIDIA toggle. Summary block matches origin §3.4.
- **Requirements**: R19.
- **Dependencies**: U8.
- **Files**:
  - `rencoder_cli/src/interact.rs`
- **Approach**: Sequential `dialoguer::Select` + `Input` + `Confirm` prompts. The summary block at the end renders with `colored` so users can verify before commit.
- **Test scenarios**: Hard to unit-test `dialoguer` directly; verify the assembled `EncodeParams` matches the user's inputs via a thin wrapper that bypasses the terminal.
- **Verification**: Manual walk-through; assert the printed summary matches each input.

### U10. Analyze subcommand

- **Goal**: `rencoder analyze` runs the analyzer with the right CLI surface (`-d`, `-f`, `-e`, `-p`, `-v`, `-o` per origin §3.4).
- **Requirements**: R18, R19.
- **Dependencies**: U7, U8.
- **Files**: `rencoder_cli/src/args.rs` (extend), `rencoder_cli/src/main.rs` (dispatch).
- **Approach**: `AnalyzeArgs` mirrors origin §3.4; main dispatches based on the subcommand. `-p` flips `AnalyzeParams.parallel`.
- **Test scenarios**:
  - `Args::try_parse_from(["rencoder", "analyze", "-d", ".", "-p", "-v"])` parses with `parallel=true, verbose=true`.
  - End-to-end against a small fixture directory produces a stats summary.
- **Verification**: CLI integration tests; smoke test.

### U11. Integration tests + documentation

- **Goal**: Cross-crate integration tests covering the full origin §6 matrix; README documents install/build/usage.
- **Requirements**: all (verification only).
- **Dependencies**: U1 through U10.
- **Files**:
  - `rencoder_lib/tests/integration.rs` (or per-feature integration test files)
  - `README.md`
- **Approach**: Spawn `rencoder_cli` as a subprocess against fixture videos; compare exit codes, output file presence, and (for the analyzer) summary text against `video_fps_stats.zsh` reference output. README sections: install (Homebrew/apt for ffmpeg + cargo build), quickstart, CLI reference, config schema.
- **Test scenarios** (origin §6):
  - Max-FPS matrix: 25/30/29.97 → no reduce; 60 → reduce.
  - Encoder-cache: `ffmpeg -encoders` runs exactly once across 100 files.
  - NVIDIA: `--nv` without `nvidia-smi` falls back; with empty output falls back; with one GPU enables.
  - Parallel analyze + `c-probe` together handle 100+ files without lock contention.
- **Verification**: `cargo test --workspace` passes; `cargo clippy --all-targets -- -D warnings` is clean; `cargo fmt --check` is clean.

### U12. Parallel analyze (optional)

- **Goal**: `analyze_fps_parallel` using `rayon::par_iter` over the file list, behind the `parallel-analyze` feature.
- **Requirements**: R11, R17, R18.
- **Dependencies**: U7.
- **Files**: `rencoder_lib/src/analyzer/fps_stats.rs` (extend), `rencoder_lib/Cargo.toml` (already declares `rayon` as optional).
- **Approach**: `rayon::prelude::*`; results merge into the single `FpsStats` via `Mutex<FpsStats>` (parking_lot) or per-thread accumulation + reduce. The `c-probe` path must be thread-safe per R17.
- **Test scenarios**:
  - 100-file parallel run produces identical `FpsStats.groups` to serial.
  - Mixed-success run: failures do not deadlock the parallel iterator.
- **Verification**: `cargo test -p rencoder_lib --features parallel-analyze` passes.

### U13. C FFI probe (optional)

- **Goal**: `c-probe` feature exposing `probe_video_metadata_fast` via C FFI to `libavformat`. Optional; default path uses `ffprobe` CLI.
- **Requirements**: R2, R17.
- **Dependencies**: U3.
- **Files**:
  - `rencoder_lib/src/ffi/mod.rs`
  - `rencoder_lib/src/ffi/video_probe.c`
  - `rencoder_lib/build.rs` (conditional on `c-probe`)
  - `rencoder_lib/Cargo.toml` (add `cc`, `pkg-config` build deps)
- **Approach**: A thin C shim around `avformat_open_input` + `avformat_find_stream_info` reads codec name, `avg_frame_rate`, `r_frame_rate` and returns a malloc'd C struct; the Rust side wraps it in `Box` and frees via `libc::free`. The bridge is stateless (R17): each call creates its own `AVFormatContext`. `pkg-config` resolves the libavformat link flags.
- **Test scenarios**:
  - `probe_video_metadata_fast(path)` returns the same `VideoMetadata` as `probe_video_metadata(path)` for each fixture.
  - 100 concurrent calls from `rayon` complete without UB (run under Miri or thread sanitizer if available).
- **Verification**: `cargo test -p rencoder_lib --features c-probe` passes; `cargo build --features c-probe --release` succeeds on Linux and macOS.

### U14. GUI prototype — SUPERSEDED by design.md §3.8

The original plan deferred this to a separate crate with a future framework choice. The implementation now lives in `rencoder_ui` (Dioxus desktop 0.6) with an actor model (Command/Event protocol). See **`docs/design.md` §3.8** for the committed design — supersedes this section.

- **Goal**: superseded.
- **Requirements**: R13 (Piped variant kept but unused by GUI; §3.8 uses `cmd.spawn()` instead).
- **Files**: third workspace crate `rencoder_ui` (per §3.1 / §3.8).
- **Approach**: Dioxus desktop 0.6, actor model with `mpsc` Command/Event channel (see §3.8.3–§3.8.6).

## Scope Boundaries

### In scope

- Workspace setup (U1).
- Library implementation per origin §3 (U2-U7, U12, U13).
- CLI per origin §3.4 (U8-U10).
- Integration tests + README (U11).
- Optional: parallel-analyze feature (U12), c-probe feature (U13).

### Deferred for later

- GUI prototype (U14) — separate crate, depends on framework choice.
- Windows support — origin marks as "subsequent"; cross-platform concerns (path separators, line endings, `nvidia-smi` availability) not addressed in this plan.

### Outside this product's identity

- `ffmpeg-next` adoption (R2 explicitly rejects).
- Replacing `walkdir` with `fd` or `xargs` with parallel shell (R10, R11).
- Audio-only or image transcoding — this tool is video-only.

## Risks & Dependencies

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| `ffmpeg`/`ffprobe` version skew across machines produces inconsistent output. | Medium | Medium | Pin a minimum version (>= 6.0) in README; integration tests assume features present. |
| `nvidia-smi` output format changes across driver versions. | Low | Low | Match a stable subset ("GPU 0: ..." line); log full output at `trace!` for debugging. |
| `walkdir` + parallel `rayon` + many files stresses I/O. | Low | Low | Phase 11 includes a perf smoke test on 100+ files; serial fallback always available. |
| `c-probe` build fails on macOS due to libavformat headers missing. | Medium | Medium | Phase 13 documents Homebrew `ffmpeg` install; CI matrix covers both platforms. |
| User passes `--nv` on a system without GPU; silent fallback surprises them. | Medium | Low | Log at `info!` ("NVIDIA requested but no GPU detected; using software encoding") so the user sees what happened. |

## Verification

End-to-end checks (CI + manual):

1. `cargo build --workspace` is clean.
2. `cargo build --release` is clean.
3. `cargo test --workspace` passes (unit + integration).
4. `cargo test -p rencoder_lib --features parallel-analyze` passes.
5. `cargo test -p rencoder_lib --features c-probe` passes.
6. `cargo clippy --all-targets --all-features -- -D warnings` is clean.
7. `cargo fmt --check` is clean.
8. `rencoder encode -f sample.mp4 -c av1 -b 45` produces a valid `sample.av1.mkv`.
9. `rencoder encode -f highfps.mp4 -c av1 --max-fps 30` produces output with fps <= 30.
10. `rencoder analyze -d ./fixtures -p -v -o stats.txt` produces a non-empty stats file.

## Sources & Research

- Origin document: `docs/smart_code.md` v5.1 — all requirements, decisions, architecture, and phases traced from this file.
- User feedback (in-memory, persisted at `~/.claude/projects/-Users-gpdi-code-rencoder/memory/max-fps-from-config.md`): `max_fps` and similar tunables must come from a JSON config file, not be hardcoded in struct fields. Drives `Config` design (U1) and `max_fps` flow through the CLI (U8).

No external research was required: the project is a 1:1 port of two shell scripts the user already operates; the standard Rust ecosystem crates (`walkdir`, `serde_json`, `rayon`, `clap`, `dialoguer`, `ctrlc`, `parking_lot`, `thiserror`, `anyhow`, `env_logger`, `colored`, `num_cpus`) are well-known and the design doc has already made the selection decisions.
