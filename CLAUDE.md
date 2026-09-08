# Rencoder

Rust rewrite of `encode.zsh` and `video_fps_stats.zsh`: smart video transcoding and analysis.

## Workspace

- `rencoder_lib` -- core library (`thiserror`, `walkdir`, `rayon`)
- `rencoder_cli` -- CLI frontend (`clap`, `dialoguer`, `ctrlc`, `parking_lot`)

Design source of truth: `docs/smart_code.md`. Update the doc first when behavior changes.

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
| `nvidia-smi` | optional | only consulted when `--nv` is passed; missing or no GPUs = silent fallback to software encoding |

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
