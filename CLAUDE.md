# Rencoder

Rust rewrite of `encode.zsh` and `video_fps_stats.zsh`: smart video transcoding and analysis.

## Workspace

- `rencoder_lib` -- core library (`thiserror`, `walkdir`, `rayon`, `toml`); `publish = false` (U18, internal lib not on crates.io)
- `rencoder_cli` -- CLI frontend (`clap`, `dialoguer`, `ctrlc`, `parking_lot`)

Design source of truth: `docs/smart_code.md`. Update the doc first when behavior changes.

## Single-directory app (U18)

Rencoder is delivered and runs as a single self-contained directory:

```
<app_dir>/                    # = current_exe().parent()
├── rencoder                  # binary
├── rencoder.toml             # config (auto-created on first run if missing)
├── log/                      # runtime logs
└── data/                     # runtime data (Parquet, etc.)
```

- All persistent state lives in `<app_dir>`; CWD is reserved for input files only.
- `$RENENCODER_CONFIG` / `$RENENCODER_LOG_DIR` / `$RENENCODER_DATA_DIR` override defaults.
- dist (via `scripts/dist.sh`) mirrors this layout — extract a tarball, get a ready-to-run app dir.

## App-level config (U18)

Only `target` (`-f`/`-d`) and `action` (`encode`/`analyze`) plus per-run
overrides (`-i`, `--config`, `-o`, passthrough args) stay on the CLI.
Everything else (format, bitrate, max-fps, crf, nv, extensions, encoder
overrides, analyze format, parallel) lives in `rencoder.toml` at the
app-level location (binary dir by default; `$RENENCODER_CONFIG` overrides).
See `rencoder_lib::config::app::AppConfig` for the schema and
`examples/rencoder.toml.example` for a dev-only reference. Unknown TOML keys are
rejected so typos surface immediately.

CRF is a `u32` (default 45, sentinel 0 = bitrate mode), not a bool — this
aligns 1:1 with ffmpeg's `-crf N`.

## Cross-platform encapsulation (U18)

Platform-specific behavior follows the **"cfg at definition, uniform
signature at call"** principle:

- `#[cfg(...)]` lives only inside the function body that does the
  platform-specific work.
- Callers see one stable signature; no cfg at the call site.
- **Current implementation is Unix only** (built and tested on macOS;
  Linux shares the `#[cfg(unix)]` branch and will be verified when
  we cross-compile or run on Linux). Non-Unix platforms are not yet
  implemented — future work.
- Adding a new platform later = edit one file (the definition site).

Concrete: `rencoder_lib::config::fs_perms` exposes
`set_config_file_mode` (Unix: `0o644`) and `set_app_dir_mode` (Unix:
`0o755`); non-Unix is a no-op placeholder.

## Build

```bash
cargo build
cargo build --release
cargo build -p rencoder_lib --features parallel-analyze
```

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## External tools

| Tool         | Required | Notes |
|--------------|----------|-------|
| `ffmpeg`     | yes      | always |
| `ffprobe`    | yes      | always |
| `nvidia-smi` | optional | only consulted when `[encode].nv = true` in `rencoder.toml`; missing or no GPUs = silent fallback to software encoding |

## Conventions

- Library errors: `thiserror`. CLI errors: `anyhow`. `EncodeError` is `#[non_exhaustive]`.
- `EncoderCache` is passed by `&` -- never cloned.
- `-r <fps>` is an FFmpeg output option: `build_ffmpeg_command` must place it after `-i`, before the output path.
- Phase 2 (this commit) covers workspace + base data structures. Subsequent phases per `docs/smart_code.md` §7.

## TODO

- `MaxFpsConfig::need_reduce` in `rencoder_lib/src/types.rs` -- the source-fps > cap predicate. Awaiting design input on epsilon tolerance semantics (strict vs. non-strict, which side of the comparison the eps lives on).

## Refactor / rename methodology

When a refactor or rename touches more than one file, the wrong pattern is
_edit one file, build fails, edit the next file, build fails, ..._ — wasteful
round trips because every file move typically requires updating several
call sites. Use this workflow instead:

1. **Map the full change set before editing.** Use `git grep` (or `rg`) to
   list every call site of the symbol/module being moved. The output is the
   file list you need to touch. For renames, also note the search pattern
   that will catch all variants (e.g. `path_util`, `output_path_for`,
   `normalize`).

2. **Plan the order.** Public-API renames first (so dependents can see the
   new name), private moves second, tests last. Document the order in a
   short todo entry so a re-entry is cheap.

3. **Use the right tool per change type:**
   - `git mv` for renames (preserves history; one `R` in `git status`).
   - `sed -i` for batch find-replace when the same rename is repeated in
     many files.
   - `Edit` only when the file has unique context that sed can't handle.
     Read the file first; verify the path is right.
   - `cargo check` once at the END of the change set, not after each
     file. One iteration goal.

4. **Verify before each commit:**
   - `git status --short` — what actually changed?
   - `git diff <file> --` — what did the edit produce?
   - `cargo test --workspace` — is the workspace green?
   - `cargo clippy --workspace --all-targets` — are there lints?

5. **When stuck, diagnose the path first.** If a file edit fails because
   "file not found", `cat` the path or run `git ls-files | grep <name>`
   to confirm what exists. Don't guess.

6. **Backwards compat bridges are fine as a step, not a goal.** Re-exports
   in the old module preserve the existing import surface so the public
   API doesn't change during a refactor. The cleanup of those bridges is
   the final stage of the refactor (here: U16 Stage 5).
