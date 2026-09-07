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

## U4. Encoder cache + selection — DONE (post-review fixes applied)

- [x] `init_encoder_cache(enable_nv) -> Result<EncoderCache, EncodeError>` in `encoder_cache.rs`
- [x] Runner-closure seam (`init_encoder_cache_impl<F>(runner: F)`) for testable subprocess mocking
- [x] `ffmpeg -encoders` once; output captured to `encoders_output`
- [x] `nvidia-smi -L` only when `enable_nv = true`; missing/empty → `info!` log, `has_nvidia = false`
- [x] `select_encoder(format, cache, config: &Config) -> Result<String, EncodeError>` in `encoder.rs`
  - **API change from plan**: plan had `config_path: Option<&Path>`; changed to `&Config` so callers load once per batch instead of per file
- [x] Preference chain: config override → NVENC → software default, with fallback
- [x] Format-to-encoder mapping: `Av1 → libsvtav1/av1_nvenc`, `X264 → libx264/h264_nvenc`, `Hevc → libx265/hevc_nvenc`, `Vp9 → libvpx-vp9`
- [x] Config override support: `Config.encoders.{av1,x264,hevc,vp9}`
- [x] Config::from_value updated to parse `encoders` block
- [x] **Post-review fixes**:
  - `parse_encoders` rejects empty-string overrides (would otherwise match any cache via `str::contains("") == true`)
  - `Config::load` rejects negative `max_fps` with `EncodeError::InvalidParam`
  - Removed dead `EncodeParams.enable_nv` field; `cache.has_nvidia` is the source of truth
  - `probe_video_metadata_auto` falls back from c-probe FFI to CLI on failure (defensive, c-probe lands in Phase 13)
- [x] Tests: 9 encoder_cache + 7 select_encoder + 8 config + 6 probe = 30 U4-related tests
- **Verify**: `cargo test -p smart_encode_lib --lib` 45/45 pass; workspace clean

## U5. Command builder + single-file transcode — DONE

- [x] `should_skip_encoding(codec, format, need_reduce) -> bool` in `command.rs`
  - 3-row decision table (R5); rejects encoder-name aliases and empty codec
- [x] `build_ffmpeg_command(task, params, encoder, src_fps) -> Vec<String>` in `command.rs`
  - argv order: `ffmpeg -y -i <in> -c:v <enc> [-preset 6] -crf X | -b:v Xk -c:a copy [-r X] [...extra] <out>`
  - CRITICAL: `-r` after `-i`, before output path (asserted by index in test)
  - R21: `src_fps = None` → no `-r` injection
  - `max_fps = 0` (disabled) → no `-r` injection
  - `src_fps` within cap → no `-r` injection
- [x] `transcode_single(task, params, cache, config, output) -> Result<TranscodeResult, EncodeError>` in `processor/batch.rs`
  - Pipeline: existence check → probe → predicate → skip-table → select → build → execute
  - Runner closure seam in `transcode_after_probe` so subprocess is mockable
  - `run_ffmpeg` honors `ProcessOutput` (Inherit/Piped/Null)
- [x] `MaxFpsConfig::need_reduce` (U2 long-standing TODO) — strict `>` with eps on right
  - 6 new tests covering §6 matrix: 25/30/29.97/60/30.06 + zero-eps boundary
- [x] `MaxFpsConfig::default()`, `VideoFormat::default`, `Target::default`, `EncodeParams::default`
  - Defaults unlock `..Default::default()` ergonomics in test fixtures
- [x] Tests added in 4 steps (per-step additions as requested):
  - Step 1: 7 need_reduce tests + 1 default test in `types.rs` (23 types tests total)
  - Step 2+3: 19 tests in `command.rs` (7 skip-table + 12 argv-layout)
  - Step 4: 7 tests in `batch.rs` (skip/reduce/no-r/fail/missing-encoder/exists)
- **Verify**: `cargo test -p smart_encode_lib --lib` 108/108 pass (was 75; +33 from U5)

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
