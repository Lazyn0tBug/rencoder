# Rencoder — TODO

Actionable task list for executing `docs/plans/2026-09-07-001-feat-rencoder-rust-port-plan.md`.

Status legend: `[ ]` not started, `[~]` in progress, `[x]` done, `[!]` blocked.

---

## U1. Foundation (workspace, types, errors, config) — DONE

- [x] Workspace `Cargo.toml` with `smart_encode_lib` + `smart_encode_cli`
- [x] `smart_encode_lib/Cargo.toml` with deps (walkdir, serde_json, log, thiserror, num_cpus, rayon opt)
- [x] `smart_encode_cli/Cargo.toml` with deps (clap, dialoguer, ctrlc, parking_lot, env_logger, anyhow)
- [x] `EncodeError` with `#[non_exhaustive]` (7 variants per origin §5)
- [x] All public types in `types.rs` (`VideoFormat`, `VideoMetadata`, `MaxFpsConfig`, `ProcessOutput`, `EncoderCache`, `EncodeParams`, `Target`, `TranscodeTask`, `TranscodeResult`, `FpsStats`, `AnalyzeParams`)
- [x] `Config` struct + `Config::load` + `Config::max_fps_config()` (5 unit tests passing)
- [x] Stub modules with phase comments: collect, ffmpeg/{probe,encoder,encoder_cache,command}, fps/{parse,limit}, analyzer/fps_stats, processor/batch
- [x] `path_util::{normalize, output_path_for}` stubs
- [x] Project CLAUDE.md (build/test, external tools, conventions, TODO)
- [x] `.gitignore` (target, .DS_Store, *.swp, *~, .claude/settings.local.json)
- [x] Initial commit (`8a35100`)
- **Verify**: `cargo check --workspace` clean, `cargo test -p smart_encode_lib --lib config` 5/5 pass

## U2. Max FPS predicate

- [!] **`MaxFpsConfig::need_reduce` predicate** — currently `todo!()`, awaiting user implementation
- [ ] Strict `>` with epsilon on right side: `src_fps > max_fps + eps`
- [ ] Unit tests in `types.rs` mirroring origin §6 matrix:
  - [ ] `60.0` @ cap=30 → `true`
  - [ ] `30.0` @ cap=30 → `false`
  - [ ] `29.97` @ cap=30, eps=0.05 → `false`
  - [ ] `25.0` @ cap=30 → `false`
- [ ] `MaxFpsConfig::default().enabled()` returns `false` test
- **Verify**: `cargo test -p smart_encode_lib --lib types` passes all `need_reduce` cases

## U3. Probing — DONE

- [x] `parse_fps_value` skeleton in `fps/parse.rs` — trim, dispatch to fraction/integer, validate
- [x] `validate_fps` helper — rejects non-finite and <= 0
- [x] `parse_fraction` — f64 parse both sides, explicit `d == 0.0` check, distinct errors
- [x] `probe_video_metadata` in `ffmpeg/probe.rs` — single `ffprobe` call
- [x] `parse_probe_output` (pure JSON mapping, split from subprocess)
- [x] `probe_video_metadata_auto` feature-gated dispatch
- [x] Unit tests: 13 parse cases (parses_fraction, parses_integer, parses_decimal, parses_decimal_with_trailing_zero, trims_whitespace, rejects_empty, rejects_zero_fraction_numerator, rejects_zero_denominator, rejects_negative_integer, rejects_negative_decimal, rejects_garbage, rejects_nan, rejects_infinity)
- [x] Unit tests: 6 probe cases (parses_full_output, parses_minimal_output, rejects_empty_streams, rejects_no_streams_key, rejects_invalid_json, parses_nominal_when_avg_missing)
- [x] Integration test: `real_ffprobe_against_fixture` (generates 1s 30fps H.264, probes, asserts h264 + ~30fps)
- **Verify**: `cargo test -p smart_encode_lib --lib` 25/25 pass

## U4. Encoder cache + selection — DONE

- [x] `init_encoder_cache(enable_nv) -> Result<EncoderCache, EncodeError>` in `encoder_cache.rs`
- [x] Runner-closure seam (`init_encoder_cache_impl<F>(runner: F)`) for testable subprocess mocking
- [x] `ffmpeg -encoders` once; output captured to `encoders_output`
- [x] `nvidia-smi -L` only when `enable_nv = true`; missing/empty → `info!` log, `has_nvidia = false`
- [x] `select_encoder(format, cache, config_path?) -> Result<String, EncodeError>` in `encoder.rs`
- [x] Preference chain: config override → NVENC → software default, with fallback
- [x] Format-to-encoder mapping: `Av1 → libsvtav1/av1_nvenc`, `X264 → libx264/h264_nvenc`, `Hevc → libx265/hevc_nvenc`, `Vp9 → libvpx-vp9`
- [x] Config override support: `Config.encoders.{av1,x264,hevc,vp9}`
- [x] Config::from_value updated to parse `encoders` block
- [x] Tests: 9 encoder_cache tests (substring match, has_nvidia flag, fallback paths, ffmpeg-once)
- [x] Tests: 7 select_encoder tests (software, NVENC, fallback, override precedence, missing)
- **Verify**: `cargo test -p smart_encode_lib --lib` 44/44 pass; workspace clean

## U5. Command builder + single-file transcode

- [ ] `build_ffmpeg_command(task, params, encoder, src_fps) -> Vec<String>` in `ffmpeg/command.rs`
- [ ] Audio codec selection (`-c:a copy` if compatible, else `libopus`/`aac`)
- [ ] CRITICAL: `-r <fps>` placement — AFTER `-i`, BEFORE output path (assert by index in test)
- [ ] R21: when `src_fps = None`, do NOT inject `-r`
- [ ] When `max_fps = 0`, do NOT inject `-r`
- [ ] `should_skip_encoding(codec, format) -> bool` with 3-row decision table (R5)
- [ ] `transcode_single(task, params, cache, output) -> Result<TranscodeResult, EncodeError>` in `processor/batch.rs`
- [ ] Single-file flow: existence check → probe → predicate → skip-table → select → build → execute
- [ ] Tests:
  - [ ] argv ordering: `-r` index > `-i` index AND `-r` index < output path index
  - [ ] `src_fps=None` → argv contains no `-r`
  - [ ] `should_skip_encoding("h264", X264)` → `true`
  - [ ] `should_skip_encoding("h264", X264)` with reduce needed → `false`
  - [ ] `transcode_single` on existing output → `SkippedExists`
- **Verify**: `cargo test -p smart_encode_lib --lib command` passes; manual end-to-end on a 5s sample produces a valid output

## U6. File collection + batch transcode

- [ ] `collect_files(target, extensions) -> Result<Vec<PathBuf>, EncodeError>` in `collect.rs`
- [ ] `walkdir::WalkDir::new(root).follow_links(false)` (R10)
- [ ] Filter by extension, case-insensitive
- [ ] `transcode_batch(params, files, cache, output, callback?)` in `processor/batch.rs`
- [ ] Failures do NOT short-circuit the batch; each file gets its own `TranscodeResult`
- [ ] Optional `result_callback` invoked once per file, in order
- [ ] Tests:
  - [ ] Symlink loop → no infinite recursion
  - [ ] Empty `extensions` → returns all files
  - [ ] Mixed file types → filtered correctly
  - [ ] Batch with one failure returns mixed results, no abort
  - [ ] Callback fires once per file in input order
- **Verify**: `cargo test -p smart_encode_lib --lib collect batch` passes; smoke test on small multi-file batch

## U7. FPS analyzer

- [ ] `analyze_fps(files: &[PathBuf]) -> Result<FpsStats, EncodeError>` in `analyzer/fps_stats.rs`
- [ ] Per file: `probe_video_metadata_auto` → `best_fps()` → `round` → `u32` → `FpsStats::add`
- [ ] Failures increment `failed_files`, log `warn!`, do NOT panic
- [ ] `FpsStats::summary()` — group counts
- [ ] `FpsStats::detailed()` — file list per group (for `-v` flag)
- [ ] Tests:
  - [ ] Empty input → `total_files=0, parsed_files=0`
  - [ ] Three distinct fps (24, 30, 60) → three groups in `BTreeMap`
  - [ ] One probe failure → `failed_files=1`, others parsed
  - [ ] `summary()` and `detailed()` non-empty strings
- **Verify**: `cargo test -p smart_encode_lib --lib fps_stats` passes; smoke test against fixture directory

## U8. CLI core

- [ ] `Args` enum in `args.rs` with `Encode(EncodeArgs)` + `Analyze(AnalyzeArgs)` subcommands (R19)
- [ ] `EncodeArgs`: `-c/--format`, `-b/--bitrate`, `-f/--file`, `-d/--dir`, `-e/--ext`, `-i/--interactive`, `--nv`, `--` for passthrough
- [ ] `signal::install_handler(Arc<Mutex<Option<PathBuf>>>)` in `signal.rs`
- [ ] `ctrlc::set_handler` removes partial file then `exit(1)`
- [ ] `log::init()` in `log.rs` — `env_logger::Builder::from_env(Env::default().default_filter_or("info"))`
- [ ] `main.rs` dispatches on subcommand; encodes → build params → init cache → install signal → batch
- [ ] Tests:
  - [ ] `Args::try_parse_from(["smart-encode", "encode", "-f", "in.mp4"])` → `Ok(Encode(...))`
  - [ ] Extra args after `--` end up in `extra_args`
  - [ ] Manual SIGINT during encode deletes partial file
- **Verify**: `cargo build -p smart_encode_cli` succeeds; manual SIGINT test passes

## U9. Interactive mode

- [ ] `interact::run_wizard() -> Result<EncodeParams>` in `interact.rs`
- [ ] `dialoguer::Select` for format
- [ ] `dialoguer::Input` for CRF/bitrate (default based on format)
- [ ] `dialoguer::Input` for target file/dir path
- [ ] `dialoguer::Input` for extensions (comma-separated, multi)
- [ ] `dialoguer::Input` for max-fps
- [ ] `dialoguer::Confirm` for `--nv`
- [ ] Summary block with `colored` styling — must include max-fps and NVIDIA state (origin §3.4)
- [ ] Confirmation prompt; cancel → `EncodeError::UserCancel`
- [ ] Test: thin wrapper that bypasses terminal and asserts assembled `EncodeParams`
- **Verify**: Manual walk-through; assert summary block contains every input

## U10. Analyze subcommand

- [ ] `AnalyzeArgs` in `args.rs`: `-d/--dir`, `-f/--file`, `-e/--ext`, `-p/--parallel`, `-v/--verbose`, `-o/--output`
- [ ] `main.rs` dispatches: builds `AnalyzeParams`, runs `analyze_fps` (or `analyze_fps_parallel` when `parallel`), prints summary, writes detailed file when `-v`
- [ ] Tests:
  - [ ] `Args::try_parse_from(["smart-encode", "analyze", "-d", ".", "-p", "-v"])` → `parallel=true, verbose=true`
  - [ ] End-to-end on fixture directory produces stats summary
- **Verify**: `cargo run -p smart_encode_cli -- analyze -d tests/fixtures -v` succeeds

## U11. Integration tests + documentation

- [ ] `smart_encode_lib/tests/` integration test files
- [ ] `tests/fixtures/` with small sample videos (a few KB each: low-fps, high-fps, already-target-codec)
- [ ] Origin §6 test matrix:
  - [ ] 25 fps → no reduce
  - [ ] 30 fps → no reduce
  - [ ] 29.97 fps → no reduce
  - [ ] 60 fps → reduce
- [ ] Encoder-cache test: 100 files, `ffmpeg -encoders` runs exactly once
- [ ] NVIDIA tests: `--nv` without nvidia-smi, with empty output, with GPU present
- [ ] Parallel + c-probe: 100+ files concurrently
- [ ] `README.md` with install (ffmpeg + cargo build), quickstart, CLI reference, config schema
- [ ] CI workflow file (`.github/workflows/ci.yml`) — build, test, clippy, fmt
- **Verify**: `cargo test --workspace` all pass; `cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo fmt --check` clean

## U12. Parallel analyze (optional)

- [ ] `analyze_fps_parallel` in `analyzer/fps_stats.rs` behind `#[cfg(feature = "parallel-analyze")]`
- [ ] `rayon::prelude::*`; merge results via `Mutex<FpsStats>` (parking_lot) or per-thread reduce
- [ ] `c-probe` interaction test: 100 concurrent probes complete without UB
- [ ] Tests: parallel output identical to serial
- **Verify**: `cargo test -p smart_encode_lib --features parallel-analyze` passes

## U13. C FFI probe (optional)

- [ ] Uncomment `pub mod ffi;` in `lib.rs` with `#[cfg(feature = "c-probe")]`
- [ ] `smart_encode_lib/ffi/video_probe.c` — thin shim around `avformat_open_input` + `avformat_find_stream_info`
- [ ] Returns codec name + `avg_frame_rate` + `r_frame_rate` via a malloc'd C struct
- [ ] `build.rs` gated on `c-probe`; uses `cc` to compile + `pkg-config` for libavformat
- [ ] Rust wrapper in `ffi/mod.rs`: `Box`-wraps the C struct, frees with `libc::free`
- [ ] `probe_video_metadata_fast` exposed when feature enabled
- [ ] `probe_video_metadata_auto` dispatches based on feature flag
- [ ] Tests: same fixture, fast and slow paths produce identical `VideoMetadata`
- [ ] macOS + Linux build verified
- **Verify**: `cargo build --features c-probe` clean; `cargo test --features c-probe` passes

## U14. GUI prototype (DEFERRED)

- [ ] Framework decision (Tauri / egui / Slint) — separate decision, separate crate
- [ ] New crate `smart_encode_gui` consuming `smart_encode_lib`
- [ ] Use `ProcessOutput::Piped` for live progress UI
- **Verify**: out of scope for this plan

---

## Open Questions

- **Q1**: Should `Config::load` validate negative `max_fps` and reject, or clamp to 0? (deferred to implementation — leaning toward reject with `InvalidParam`)
- **Q2**: For `analyze_fps`, should the analyzer also output the codec alongside the fps in detailed mode? (deferred — current design only outputs fps; codec tracking would extend `FpsStats.groups` value type)
- **Q3**: Does Phase 11's CI need macOS runners in addition to Linux? (deferred — depends on whether `c-probe` is gated to Linux-only)
