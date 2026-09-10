# Rencoder — TODO

Actionable task list for executing `docs/plans/2026-09-07-001-feat-rencoder-rust-port-plan.md`.

Status legend: `[ ]` not started, `[~]` in progress, `[x]` done, `[!]` blocked.

---

## U1. Foundation (workspace, types, errors, config) — DONE

- [x] Workspace `Cargo.toml` with `rencoder_lib` + `rencoder_cli`
- [x] `rencoder_lib/Cargo.toml` with deps (walkdir, serde_json, log, thiserror, num_cpus, rayon opt)
- [x] `rencoder_cli/Cargo.toml` with deps (clap, dialoguer, ctrlc, parking_lot, env_logger, anyhow)
- [x] `EncodeError` with `#[non_exhaustive]` (7 variants per origin §5)
- [x] All public types in `types.rs` (`VideoFormat`, `VideoMetadata`, `MaxFpsConfig`, `ProcessOutput`, `EncoderCache`, `EncodeParams`, `Target`, `TranscodeTask`, `TranscodeResult`, `FpsStats`, `AnalyzeParams`)
- [x] `Config` struct + `Config::load` + `Config::max_fps_config()` (5 unit tests passing)
- [x] Stub modules with phase comments: collect, ffmpeg/{probe,encoder,encoder_cache,command}, fps/{parse,limit}, analyzer/fps_stats, processor/batch
- [x] `path_util::{normalize, output_path_for}` stubs
- [x] Project CLAUDE.md (build/test, external tools, conventions, TODO)
- [x] `.gitignore` (target, .DS_Store, *.swp, *~, .claude/settings.local.json)
- [x] Initial commit (`8a35100`)
- **Verify**: `cargo check --workspace` clean, `cargo test -p rencoder_lib --lib config` 5/5 pass

## U2. Max FPS predicate

- [!] **`MaxFpsConfig::need_reduce` predicate** — currently `todo!()`, awaiting user implementation
- [ ] Strict `>` with epsilon on right side: `src_fps > max_fps + eps`
- [ ] Unit tests in `types.rs` mirroring origin §6 matrix:
  - [ ] `60.0` @ cap=30 → `true`
  - [ ] `30.0` @ cap=30 → `false`
  - [ ] `29.97` @ cap=30, eps=0.05 → `false`
  - [ ] `25.0` @ cap=30 → `false`
- [ ] `MaxFpsConfig::default().enabled()` returns `false` test
- **Verify**: `cargo test -p rencoder_lib --lib types` passes all `need_reduce` cases

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
- **Verify**: `cargo test -p rencoder_lib --lib` 25/25 pass

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
- **Verify**: `cargo test -p rencoder_lib --lib` 45/45 pass; workspace clean

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
- **Verify**: `cargo test -p rencoder_lib --lib` 108/108 pass (was 75; +33 from U5)

## U6. File collection + batch transcode — DONE

- [x] `collect_files(target, extensions) -> Result<Vec<PathBuf>, EncodeError>` in `collect.rs`
- [x] `walkdir::WalkDir::new(root).follow_links(false)` (R10)
- [x] `entry.file_type().is_file()` (NOT `Path::is_file`) so symlinks to files are excluded
- [x] Case-insensitive extension allowlist; empty allowlist = all files
- [x] `Target::File` returns the file (filtered) or empty; `Target::Dir` walks
- [x] `transcode_batch(params, files, cache, config, output, callback?)` in `processor/batch.rs`
- [x] Processor closure seam (`transcode_batch_with_processor`) so tests inject canned results
- [x] Failures do NOT short-circuit the batch; per-file errors become `TranscodeResult::Failed`
- [x] `result_callback` fires once per file in input order, after the result is recorded
- [x] Output path derived via `output_path_for(input, format)` per file
- [x] Tests added per step:
  - collect: 12 tests (file-target 5 cases, dir-target 7 cases including symlink file + loop guards)
  - batch: 7 tests (order preserved, failure-doesn't-abort, callback in order, no-callback path, empty list, output-path derivation, public-API sanity)
- **Verify**: `cargo test -p rencoder_lib --lib` 127/127 pass (was 108; +19 from U6)

## U7. FPS analyzer — DONE (initial; refactored under U7.5)

- [x] `FpsStats::summary()` in `types.rs` — group counts plus totals line
- [x] `FpsStats::detailed()` in `types.rs` — per-file listing grouped by fps
- [x] `analyze_fps(files) -> Result<FpsStats, EncodeError>` in `analyzer/fps_stats.rs`
  - Probe seam (`analyze_fps_with<F>(probe: F)`) so tests inject canned metadata
  - Per-file failures (probe Err OR no fps in metadata) increment `failed_files` with `warn!` log
  - fps rounded to nearest u32 (NTSC 23.976 -> 24, 29.97 -> 30, 59.94 -> 60)
- [x] Tests added per step:
  - Step 1: 5 FpsStats::summary/detailed tests in `types.rs` (group counts, totals, empty, failures, path grouping)
  - Step 2: 7 analyze_fps tests in `analyzer/fps_stats.rs` (empty input, group-by-fps, integer fps, probe failure, missing fps, mixed success+failure, boundary rounding)
- **Verify**: `cargo test -p rencoder_lib --lib` 139/139 pass (was 127; +12 from U7)

## U7.5. Polars-based analyzer refactor — DONE

- [x] Add `polars = { version = "0.46", features = ["lazy", "csv"] }` to `rencoder_lib/Cargo.toml`
- [x] `AnalysisRecord` struct: `path`, `fps: Option<u32>`, `codec: Option<String>`, `duration_secs: Option<f64>`, `file_bytes: u64`
- [x] `analyze_records(files) -> Result<Vec<AnalysisRecord>, EncodeError>` — per-file probe + record (failed probes still emit a row with None metric fields)
- [x] `analyze_fps` refactored to call `analyze_records` then aggregate via Polars `group_by + agg` (count, sum duration_secs); FpsStats is the rendered output
- [x] `records_to_csv(records) -> String` — proper header + CSV-quoted rows
- [x] Tests: existing 12 analyzer tests still green; new Polars-specific tests confirm aggregation parity
- [x] Integration: `analyze` CLI now emits per-file CSV to stdout + one-line summary to stderr by default
- [x] CLI flags: `--format text` for legacy summary; `-o FILE` to redirect CSV
- [x] `docs/design.md` updated: tech decision table, analyzer design subsection, test matrix
- **Verify**: `cargo test --workspace` still 200+/200+; `cargo clippy --workspace --all-targets` clean.

## U8. CLI core

- [ ] `Args` enum in `args.rs` with `Encode(EncodeArgs)` + `Analyze(AnalyzeArgs)` subcommands (R19)
- [ ] `EncodeArgs`: `-c/--format`, `-b/--bitrate`, `-f/--file`, `-d/--dir`, `-e/--ext`, `-i/--interactive`, `--nv`, `--` for passthrough
- [ ] `signal::install_handler(Arc<Mutex<Option<PathBuf>>>)` in `signal.rs`
- [ ] `ctrlc::set_handler` removes partial file then `exit(1)`
- [ ] `log::init()` in `log.rs` — `env_logger::Builder::from_env(Env::default().default_filter_or("info"))`
- [ ] `main.rs` dispatches on subcommand; encodes → build params → init cache → install signal → batch
- [ ] Tests:
  - [ ] `Args::try_parse_from(["rencoder", "encode", "-f", "in.mp4"])` → `Ok(Encode(...))`
  - [ ] Extra args after `--` end up in `extra_args`
  - [ ] Manual SIGINT during encode deletes partial file
- **Verify**: `cargo build -p rencoder_cli` succeeds; manual SIGINT test passes

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
  - [ ] `Args::try_parse_from(["rencoder", "analyze", "-d", ".", "-p", "-v"])` → `parallel=true, verbose=true`
  - [ ] End-to-end on fixture directory produces stats summary
- **Verify**: `cargo run -p rencoder_cli -- analyze -d tests/fixtures -v` succeeds

## U11. Integration tests + documentation

- [ ] `rencoder_lib/tests/` integration test files
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

## U8. CLI core — DONE (stubs only)

- [x] `Cli` struct + `Command` enum (Encode/Analyze) in `args.rs`
- [x] `EncodeArgs`: `-c/--format` (ValueEnum: x264/hevc/vp9/av1, default av1), `-b/--bitrate`, `-f/--file`, `-d/--dir`, `-e/--ext`, `-i/--interactive`, `--nv`, `--max-fps`, `--crf`, `--config`, `--` passthrough
- [x] `AnalyzeArgs`: `-d/--dir`, `-f/--file`, `-e/--ext`, `-p/--parallel`, `-v/--verbose`, `-o/--output` (default fps_stats.txt)
- [x] `signal::install_handler()` + `register_partial` / `clear_partial` / `remove_partial` helpers
  - `ctrlc::set_handler` removes the registered partial file and exits 1
  - `Arc<Mutex<Option<PathBuf>>>` (parking_lot, no poisoning — per R14)
  - Module-level `#[allow(dead_code)]`; U11 wires main.rs into the per-batch loop
- [x] `tracing_init::init()` (CLI uses `tracing` not `log`)
  - `tracing-log` bridge captures the library's `log` events into tracing
  - `RUST_LOG` env filter, default `info`, no target/time prefix
- [x] `main.rs`: parses args, dispatches on subcommand, both `encode_cmd` / `analyze_cmd` are stubs returning Ok — Phase 11 fills in the pipeline
- [x] Tests (28 new in this CLI crate, all passing):
  - args: 21 tests covering every flag, format variants, `--` passthrough, subcommand errors
  - signal: 5 tests for register/clear/replace/remove helpers + install_handler smoke
  - tracing_init: 1 test for idempotent init
- **Verify**: `cargo test --workspace` 176/176 pass. `cargo build --workspace --all-targets` clean.

## U10. Analyze subcommand — DONE

- [x] `analyze_cmd(args)` in `main.rs`: Target resolution -> collect_files -> analyze_fps -> print summary
- [x] `target_from_analyze_args(args)` helper: --file > --dir > current_dir (testable)
- [x] Verbose mode writes `stats.detailed()` to `args.output` (default `fps_stats.txt`)
- [x] `--parallel` warns (U12 not implemented yet) and falls back to serial
- [x] Tests:
  - `target_from_file_flag_wins`: --file beats --dir
  - `target_from_dir_flag`: --dir alone gives Target::Dir
  - `target_defaults_to_current_dir`: no flag -> cwd
  - `target_path_helper_returns_inner_path`: Target::File/Dir unwrap to inner PathBuf
- **Verify**: 4 new main.rs tests pass; workspace tests still 176/176.

## U11. Integration tests + documentation — DONE

- [x] `encode_cmd` fully wired (Config::load → init_encoder_cache → signal::install_handler → collect_files → transcode_batch → summary)
- [x] `From<FormatArg> for VideoFormat` impl in main.rs
- [x] `print_encode_summary` prints Total/Success/Reduced/Skipped/Failed counts
- [x] `rencoder_lib/tests/integration.rs` — 6 E2E tests:
  - probe_real_video_returns_codec_and_fps
  - analyze_fps_groups_mixed_fixtures (24/30/60 fps fixtures)
  - analyze_fps_handles_probe_failure_for_missing_file
  - transcode_batch_end_to_end_produces_output_files (AV1)
  - transcode_batch_skips_when_output_exists
  - init_encoder_cache_works_without_nvidia_smi
- [x] `rencoder_cli/tests/cli.rs` — 9 subprocess tests:
  - version_prints_and_exits_zero
  - top_level_help_prints_subcommands
  - encode_help_prints_options
  - analyze_help_prints_options
  - unknown_subcommand_exits_nonzero
  - analyze_subcommand_produces_stats_for_real_videos
  - analyze_subcommand_writes_verbose_file
  - encode_subcommand_produces_output_file
  - encode_rejects_missing_target
- [x] `README.md` — install, quickstart, CLI reference (encode + analyze), config schema, logging, development
- **Verify**: `cargo test --workspace` 200/200 pass (was 185; +6 lib integration + +9 CLI integration).

## U12. Parallel analyze — DONE

- [x] `analyze_records_parallel` in `analyzer/fps_stats.rs` behind `#[cfg(feature = "parallel-analyze")]`
- [x] `rayon::prelude::*`; per-record `par_iter().map().collect()` preserves input order
- [x] Per-thread probe failures become None-metric rows (same contract as serial)
- [x] Test seam `analyze_records_with_probe_parallel` for tests with `Fn + Sync + Send` bound
- [x] CLI: `analyze -p` now actually invokes the parallel path (the "not yet implemented" warning is gone)
- [x] `AnalysisRecord` derives `PartialEq` so serial-vs-parallel can be compared directly
- [x] Tests added (4, all behind the feature flag):
  - `parallel_output_matches_serial_sorted_by_path` (sorted by path; output identical)
  - `parallel_failure_becomes_none_row_not_error` (single failed probe → None row, batch continues)
  - `parallel_aggregate_matches_serial` (Polars table identical)
  - `parallel_returns_records_in_input_order` (rayon par_iter preserves order)
- **Verify**: `cargo test --workspace` 207/207; with `--features parallel-analyze` 157/157. `cargo clippy --workspace --all-targets` clean.
- **Note**: c-probe interaction test (R17) deferred to U13 when the FFI bridge lands; the parallel path will pick it up automatically via `probe_video_metadata_auto`'s feature-gated dispatch.

## U13. C FFI probe — DONE

- [x] `Cargo.toml`: c-probe feature now depends on `dep:cc` and `dep:pkg-config` (optional build deps); dev-deps unchanged
- [x] `build.rs`: when c-probe enabled, invokes `pkg-config --cflags --libs libavformat` directly, parses `-I` (passed to cc) and `-l`/`-L` (emitted as cargo link directives). Sidesteps the `pkg-config` crate's `.includes()` integration with `cc-rs` which didn't reliably propagate include paths here.
- [x] `src/ffi/video_probe.c`: stateless libavformat bridge. Allocates a fresh `AVFormatContext` per call (rayon-safe per R17); returns malloc'd CStrings for codec + avg_frame_rate + r_frame_rate; failure (open failed / no streams) returns a zeroed struct so the Rust side sees None fields.
- [x] `src/ffi/mod.rs`: `#[repr(C)]` ProbeResult mirroring the C side; reads each field, parses fps strings via the existing `parse_fps_value`, frees the C result via `rencoder_free`. Returns `Err(Probe(...))` only on NUL-in-path or calloc failure.
- [x] `lib.rs`: `pub mod ffi;` gated on `c-probe`.
- [x] CLI probe invocation: added `-show_entries format=duration` so `format.duration` is requested alongside the per-stream fields; `parse_probe_output` falls back to `format.duration` when `streams[0].duration` is missing (lavfi-generated fixtures put it there).
- [x] Integration tests (gated on c-probe feature, run with `--features c-probe`):
  - `c_probe_matches_cli_probe_for_real_video`: codec matches exactly, fps within 0.5, duration within 0.5s.
  - `c_probe_handles_missing_file_gracefully`: nonexistent path returns `Ok(VideoMetadata::default())` (all None fields), no panic.
- **Verify**: `cargo test --workspace` 207/207; with `--features c-probe` 155/155. `cargo clippy --workspace --all-targets --features c-probe` clean.


- [ ] Uncomment `pub mod ffi;` in `lib.rs` with `#[cfg(feature = "c-probe")]`
- [ ] `rencoder_lib/ffi/video_probe.c` — thin shim around `avformat_open_input` + `avformat_find_stream_info`
- [ ] Returns codec name + `avg_frame_rate` + `r_frame_rate` via a malloc'd C struct
- [ ] `build.rs` gated on `c-probe`; uses `cc` to compile + `pkg-config` for libavformat
- [ ] Rust wrapper in `ffi/mod.rs`: `Box`-wraps the C struct, frees with `libc::free`
- [ ] `probe_video_metadata_fast` exposed when feature enabled
- [ ] `probe_video_metadata_auto` dispatches based on feature flag
- [ ] Tests: same fixture, fast and slow paths produce identical `VideoMetadata`
- [ ] macOS + Linux build verified
- **Verify**: `cargo build --features c-probe` clean; `cargo test --features c-probe` passes

## U15. EncodingRecord data model + storage layer — IN PROGRESS (branch: feat/encoding-record-and-storage)

User asked for work-analysis output (not video-analysis). The data model
restructures around four workflow dimensions, and storage moves to
Parquet-as-canonical with JSONL streaming.

### Data model (4 dimensions)

- **Source** (input): source_path, source_size_bytes, source_codec,
  source_container, source_resolution, source_fps, source_duration_secs,
  source_bit_rate_bps
- **Process** (work): started_at, ended_at, encoding_duration_secs,
  encoder_selected, encoder_settings (is_crf, crf_or_bitrate, preset,
  max_fps_target), ffmpeg_argv, ffmpeg_stderr_tail
- **Result** (output): output_path, output_size_bytes, output_duration_secs,
  output_codec, compression_ratio (output_size/source_size, stored column)
- **Status** (independent): outcome (Succeeded | ReducedFps | Skipped(*)
  | Failed), error_category, error_message

### Storage strategy

- **Default stdout**: JSONL (one full record per line, all fields, nulls
  omitted) — stream-friendly, pipe to jq/duckdb.
- **`-o path.parquet`**: Parquet with Snappy compression, embedded schema;
  one file per CLI run, named `./data/encoding-<UTC-ISO8601>Z.parquet`
  (auto-generated if `-o` omitted).
- **`-o path.jsonl`**: NDJSON, also file-per-run.
- **`-o path.csv`**: legacy compat (subset of columns).

### Single DataFrame intermediate, thin writers

`records_to_dataframe(records) → DataFrame` is the schema anchor.
Three writers consume the DataFrame (not the records):

- `dataframe_to_csv(df) → String`
- `dataframe_to_jsonl(df) → String`
- `dataframe_to_parquet(df) → Vec<u8>`

No JSONL → Parquet round-trip (wasteful parse + loses Polars column types).
Schema evolution: edit `records_to_dataframe` once.

### Implementation steps (Pass 1 = data + writers; Pass 2 = integration)

**Pass 1 — data model + writers**:
- [ ] Add `EncodingRecord`, `EncoderSettings`, `EncodingOutcome`,
      `SkippedReason`, `ErrorContext`, `ErrorCategory` to `rencoder_lib/src/types.rs`
- [ ] Extend `VideoMetadata` with the source fields (resolution,
      container, bit_rate, mtime)
- [ ] Update `probe_video_metadata_fast` (lib/ffmpeg/video_probe.c) and
      `parse_probe_output` (lib/ffmpeg/probe.rs) to extract the new ffprobe
      fields
- [ ] Refactor `transcode_single` and `transcode_batch` to capture
      timing (start/end), encoder settings used, output size, ffmpeg stderr
      tail; return `Vec<EncodingRecord>` instead of
      `Vec<(PathBuf, TranscodeResult)>`
- [ ] Refactor `records_to_csv` and `records_to_jsonl` to consume
      `DataFrame` (not records directly) — single canonical schema path
- [ ] Add `records_to_parquet` via `dataframe_to_parquet` (Polars
      `ParquetWriter` with Snappy compression)
- [ ] Update CLI: `-o PATH` extension determines format; default
      timestamped Parquet in `./data/`
- [ ] Update CLI summary: end-of-batch aggregate (total files, total
      bytes in/out, throughput, failure rate by category)
- [ ] Tests: new EncodingRecord field capture; DataFrame writer
      round-trip (Parquet can be read back with same values);
      timestamp default naming; legacy tests still pass
- [ ] Document in `docs/design.md` (already updated §2.4 decision;
      needs §3 type/API listings too)

**Pass 2 — observability & archival**:
- [ ] `--log-file PATH` flag for append-mode NDJSON monitoring log
      (NDJSON is append-friendly; Parquet isn't)
- [ ] Multi-batch query examples in README (DuckDB glob over
      `./data/*.parquet`)
- [ ] README updates: replace CSV-default examples with JSONL or
      Parquet per use case

**Verify**: `cargo test --workspace` 200+/200+; with
`cargo build --release` binary size still reasonable (Polars + parquet
writer adds ~5-10 MB); `cargo clippy --workspace --all-targets` clean.


- [ ] Framework decision (Tauri / egui / Slint) — separate decision, separate crate
- [ ] New crate `rencoder_gui` consuming `rencoder_lib`
- [ ] Use `ProcessOutput::Piped` for live progress UI
- **Verify**: out of scope for this plan

---

## Open Questions

- **Q1**: Should `Config::load` validate negative `max_fps` and reject, or clamp to 0? (deferred to implementation — leaning toward reject with `InvalidParam`)
- **Q2**: For `analyze_fps`, should the analyzer also output the codec alongside the fps in detailed mode? (deferred — current design only outputs fps; codec tracking would extend `FpsStats.groups` value type)
- **Q3**: Does Phase 11's CI need macOS runners in addition to Linux? (deferred — depends on whether `c-probe` is gated to Linux-only)

---

## U16. Refactor: data / encoder / pipeline split — PENDING CONFIRMATION (branch: refactor/data-encoder-pipeline)

Design agreed (U16 split). Five top-level modules after the refactor:
`config/`, `utils/`, `data/`, `pipeline/`, `encoder/`. Old `ffmpeg/`,
`analyzer/`, `fps/` fold into the new modules. `c-probe/` is renamed from
`ffi/`. Public API stays stable (renames + module paths only); all
existing tests keep passing.

Rationale (key points from the design conversation):
- **data/** is the work-record layer (work-analysis, not video-analysis)
- **encoder/** is the ffmpeg-specific work (build argv, select codec)
- **pipeline/** is orchestration (collect → batch loop → record + log)
- **utils/** is shared infrastructure (fs, time)
- YAGNI: no `data engine/` until there's an actual query surface

### Staged checklist (each step keeps `cargo test --workspace` green)

**Stage 1 — data/ module extraction**: [x] DONE (commit b62463e)
- [x] Create `data/extract.rs` from `processor/batch.rs` (probe_video_metadata
      call, stderr capture, output size capture)
- [x] Create `data/record.rs` from `processor/batch.rs` (EncodingRecord
      assembly, encoding_settings_from_params helper, make_skipped_record
      / make_failed_probe_record constructors)
- [x] Create `data/store.rs` from `encoding/output.rs` (records_to_dataframe,
      records_to_csv / jsonl / parquet, dataframe_to_* helpers)
- [x] `lib.rs`: register `pub mod data;` with `mod extract; mod record;
      mod store;` and re-export `data::EncodingRecord` at crate root
- [x] Verify: `cargo test -p rencoder_lib --lib` passes (existing tests
      still pass; only module paths change)

**Stage 2 — pipeline/ module**: [x] DONE (commit 21927d8)
- [x] Create `pipeline/cache.rs` from `ffmpeg/encoder_cache.rs`
      (`init_encoder_cache` + private runner seam)
- [x] Create `pipeline/batch.rs` from `processor/batch.rs` (transcode_batch,
      transcode_single, build_argv / run_ffmpeg_with_stderr helpers,
      transcode_batch_with_processor test seam)
- [x] `lib.rs`: register `pub mod pipeline;` with `mod batch; mod cache;`
- [x] Update `processor/batch.rs` → `pipeline/batch.rs` callers
- [x] Verify: `cargo test -p rencoder_lib --lib` passes; integration
      tests still pass

**Stage 3 — encoder/ module**: [x] DONE (commit a4ede96)
- [x] Create `encoder/command.rs` from `ffmpeg/command.rs`
      (`build_ffmpeg_command`, `should_skip_encoding`)
- [x] Create `encoder/select.rs` from `ffmeg/encoder.rs` (`select_encoder`)
- [x] `lib.rs`: register `pub mod encoder;` with `mod command; mod select;`
- [x] Update callers in `pipeline/batch.rs`
- [x] Verify: `cargo test -p rencoder_lib --lib` passes

**Stage 4 — utils/ module**: [x] DONE (commit a256169)
- [x] Create `utils/fs.rs` from `path_util.rs` (normalize,
      output_path_for, plus any helpers used in collect.rs)
- [ ] Create `utils/time.rs` (UTC timestamp formatting helper)
- [ ] `lib.rs`: register `pub mod utils;` with `mod fs; mod time;` and
      re-export `utils::normalize` at crate root if needed
- [ ] Verify: `cargo test --workspace` passes

**Stage 5 — cleanup**: [x] DONE (commit 941a01e)
- [x] Delete old modules: `ffmeg/`, `analyzer/`, `fps/`, `processor/`,
      `path_util.rs` (when all references are migrated)
- [x] Rename `ffi/` → `c-probe/`
- [x] `lib.rs`: remove old `pub mod` declarations; ensure only new modules
      remain
- [x] Verify: `cargo build --workspace --all-targets` clean;
      `cargo test --workspace` passes; `cargo clippy --workspace
      --all-targets` clean

**Stage 6 — finalize**:
- [ ] Update README with the new layout diagram
- [ ] Verify no public API rename (callers should still work)
- [ ] Final test sweep across all features
- [ ] Tag the refactor commit

### Constraints (apply to every stage)

- **No public API rename.** Internal file moves are OK; pub use re-exports
  keep the same paths for downstream callers.
- **`cargo test --workspace` green after each commit.** Don't accumulate
  failures across stages.
- **One commit per stage** (or per logical chunk within a stage).
- **Clippy clean** at each commit.

### Migration safety

- Each `git mv` is tracked by git's rename detection (1 commit = move +
  edits, not delete + add).
- Test runners (`tests/integration.rs`, `tests/cli.rs`) use module paths
  like `crate::types::EncodingRecord` — those work regardless of file
  location; only `use crate::ffmpeg::probe_video_metadata_auto` style imports
  need updating.
- C bridge (`src/ffi/`) is renamed to `src/c-probe/` in Stage 5 only
  (the feature flag wiring in `Cargo.toml` updates once).

**Verify (final)**: `cargo test --workspace` 200+/200+;
`cargo clippy --workspace --all-targets` clean; `cargo build --release`
binary size stable; public API unchanged.

## U17. CLI / config split — DONE (branch: feat/cli-config-split)

User asked: keep only `target` (`-f`/`-d`) and `action` (`encode` /
`analyze`) on the CLI; everything else moves to a TOML config file. A
new `ProjectConfig` schema replaces the old JSON `Config` as the
user-facing surface; the runtime `Config` (used by `batch::transcode_batch`
and `encoder::select::select_encoder`) is now built from
`ProjectConfig.encode` via `Config::from_encode_config`.

### Schema (TOML)

```toml
[encode]
format = "av1"             # x264 | hevc | vp9 | av1
bitrate = 5000             # Kbps in bitrate mode; CRF value when crf=true
crf = false
max_fps = 0                # 0 disables; else 24, 25, 30, 60
nv = false
extensions = ["mp4", "mov", "mkv"]

[encode.encoders]
av1 = "libsvtav1"          # per-format override (optional)
x264 = "libx264"
hevc = "libx265"
vp9 = "libvpx-vp9"

[analyze]
format = "csv"             # csv | table | text | json
parallel = false
extensions = []
output = "fps_stats.txt"   # CLI -o wins per-run
```

### CLI surface (U17)

```
rencoder encode   -f FILE | -d DIR [-i] [--config PATH] [-o OUT] [-- EXTRA...]
rencoder analyze  -d DIR  | -f FILE      [--config PATH] [-o OUT]
```

Stripped from CLI (now in config): `-c`/`--format`, `-b`/`--bitrate`,
`--max-fps`, `--crf`, `--nv`, `-e`/`--ext`, `-p`/`--parallel`.

### Files touched

- `rencoder_lib/Cargo.toml` — added `toml = "0.8"` + `serde` derive
- `rencoder_lib/src/config/app.rs` — new: `ProjectConfig`,
  `EncodeConfig`, `AnalyzeConfig`, `EncodersConfig`, `FormatArg`,
  `AnalyzeFormatArg`, `ConfigError`, `load` / `validate` / `max_fps` /
  `encoders`
- `rencoder_lib/src/config/mod.rs` — added `Config::from_encode_config` and
  re-exports of the project module's public types
- `rencoder_cli/src/args.rs` — stripped moved flags; tests cover both
  the kept surface and the explicit rejections of removed flags
- `rencoder_cli/src/main.rs` — loads `ProjectConfig`, merges into
  `EncodeParams` / analyze command; default config path
  `./rencoder.toml`
- `examples/rencoder.toml.example` — reference config (every field documented)

### Verify

- `cargo test --workspace` green
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --check` clean
- New unit tests cover: missing config file → defaults, minimal TOML
  parsing, invalid `max_fps` rejection, CLI merge precedence (CLI >
  config > defaults), unknown-keys rejection, and removed-flag
  rejections at the clap layer.

---

## U18. Single-directory app + app-level config — PENDING CONFIRMATION (branch: refactor/single-dir-app)

User asked: pivot from project-level config (U17) to app-level config that
lives in the same directory as the binary. All persistent state (config,
log, data) lives under `<app_dir>`; the deployed artifact is a single
self-contained directory per target triple. Workspace structure kept
(lib + cli as thin wrappers; future gui/tauri follow same pattern); lib
marked `publish = false`.

### Architecture contract

- **Source**: workspace unchanged (`rencoder_lib` + `rencoder_cli`).
  `rencoder_lib/Cargo.toml` adds `publish = false` (internal lib, not on
  crates.io).
- **Runtime**: binary resolves `app_dir = current_exe().parent()`. Config,
  log, data all derive from `app_dir` by default.
- **Env overrides** (priority highest → lowest):
  `RENENCODER_CONFIG` > `<app_dir>/rencoder.toml`,
  `RENENCODER_LOG_DIR` > `<app_dir>/log/`,
  `RENENCODER_DATA_DIR` > `<app_dir>/data/`.
- **Auto-create**: missing config → atomic write of defaults + inline
  comments, stderr notice; existing-but-invalid → error, never overwrite.
- **CRF schema**: `Option<bool>` → `Option<u32>`, default `Some(45)`,
  sentinel `0` = bitrate mode. Aligns 1:1 with ffmpeg `-crf N`.
- **dist**: `scripts/dist.sh` assembles `dist/<app>-<triple>/` with
  binary + LICENSE + README + `rencoder.toml` seed + empty `log/` + `data/`.
  Mirrors runtime layout — extract and run.

### Tasks

- [ ] `rencoder_lib/Cargo.toml` — add `publish = false`
- [ ] `rencoder_lib/src/config/path.rs` — new module: `resolve()` =
  `$RENENCODER_CONFIG` or `<app_dir>/rencoder.toml`
- [ ] `rencoder_lib/src/config/fs_perms.rs` — new module with
  `cfg`-at-definition pattern: `set_config_file_mode` (Unix: `0o644`;
  non-Unix: no-op, TBD future work) and `set_app_dir_mode`
  (Unix: `0o755`; non-Unix: no-op, TBD future work). Currently built
  and tested on macOS only; Linux shares `#[cfg(unix)]` branch and
  will be verified when we cross-compile / run on Linux. Non-Unix
  placeholder is `let _ = path; Ok(())` so missing impl is a
  compile-time fact, not a runtime surprise.
- [ ] `rencoder_lib/src/config/app.rs` — `AppConfig` (renamed from `ProjectConfig`);
  `load` adds auto-create dance (write defaults with atomic tmp+rename
  if missing; error if existing-but-invalid; calls `fs_perms::set_config_file_mode`
  on the tmp file before rename)
- [ ] `rencoder_lib/src/config/app.rs` — `crf: Option<bool>` →
  `Option<u32>` (default `Some(45)`, validate `0..=63`)
- [ ] `rencoder_lib/src/config/mod.rs` — update re-exports
  (`AppConfig` (renamed from `ProjectConfig`)); `Config::from_encode_config` adapts crf
  semantics (crf > 0 → pass as `-crf N`; crf == 0 → use `bitrate` for
  `-b:v`)
- [x] `rencoder_cli/src/main.rs` — `load_app_config` (renamed from
  `load_project_config`); `DEFAULT_CONFIG_PATH` removed
  (`path::resolve_config` owns it); `--config` semantics unchanged;
  tests updated for new behavior
- [ ] `rencoder_cli/src/tracing_init.rs` — `./log/` → `<app_dir>/log/`
  (via `RENENCODER_LOG_DIR` override)
- [ ] `rencoder_cli/src/main.rs` — `./data/` encoding output →
  `<app_dir>/data/` (via `RENENCODER_DATA_DIR` override)
- [ ] `rencoder_lib/src/encoder/command.rs` (or wherever
  `build_ffmpeg_command` lives) — crf semantics updated for new schema
- [ ] `examples/rencoder.toml.example` — header updated: "dev-only reference;
  dist users don't need this"; crf value updated to `45`
- [ ] `scripts/dist.sh` — new: cargo build + assemble dist tree +
  empty log/data placeholders + cargo-dist tarball
- [ ] `docs/design.md` — already updated (§1.2, §2, §3.4.5, §3.6)
- [ ] `CLAUDE.md` — already updated (workspace note, single-directory
  section, U18 wording)

### Verify

- `cargo test --workspace` green (existing tests + new auto-create coverage)
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --check` clean
- Manual: `cargo dist build --target aarch64-apple-darwin` produces
  tarball with `rencoder`, `rencoder.toml`, `log/`, `data/`, `LICENSE`,
  `README.md`. Extract → `./rencoder --help` works from any CWD.
- Manual: `rm <app_dir>/rencoder.toml && <app_dir>/rencoder encode -d
  /some/videos` re-creates config + processes videos.
- Manual: `RENENCODER_CONFIG=/tmp/c.toml <app_dir>/rencoder ...` uses
  override; `RENENCODER_LOG_DIR=/tmp/log <app_dir>/rencoder ...` writes
  logs to `/tmp/log`.
- Existing examples/rencoder.toml.example parses with new u32 crf.
