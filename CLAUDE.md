# Rencoder

Rust rewrite of `encode.zsh` and `video_fps_stats.zsh`: smart video transcoding and analysis.

## Workspace

- `rencoder_lib` -- core library (`thiserror`, `walkdir`, `rayon`, `toml`); `publish = false` (U18, internal lib not on crates.io)
- `rencoder_cli` -- CLI frontend (`clap`, `dialoguer`, `parking_lot`; `signal-hook` on Unix, `windows-sys` on Windows)

Design source of truth: `docs/design.md`. Update the doc first when behavior changes.

**Doc scope split:**
- `docs/design.md` describes **WHAT/WHY** (architecture, contracts, constraints, design rationale). New abstractions get a one-line entry in §3.8.6's "新增" table.
- Implementation details (signatures, struct fields, work flows, thresholds) live in **`///` rustdoc comments** in the code itself. Generated with `cargo doc --no-deps`. Reviewers should reject design-doc additions that duplicate what rustdoc already says.

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

Exception (CLI only): `rencoder_cli::signal` has a Windows branch --
Unix uses `signal-hook` (SIGINT/SIGTERM/SIGHUP), Windows uses
`SetConsoleCtrlHandler` (console events). The divergence stays in that
one file; `install_handler()` keeps one signature everywhere. The
Windows path is written but **not compiler-verified here** (this
machine's toolchain has no Windows std) -- verify it on a Windows host.

## Build

```bash
cargo build
cargo build --release
cargo build -p rencoder_lib --features parallel-analyze
```

`c-probe` (native libavformat probing) is **opt-in**: it needs pkg-config +
libavformat dev headers, so it is off by default and the default build
probes via the `ffprobe` subprocess. Enable it on a host that has the
dev packages:

```bash
cargo build --features c-probe        # macOS: brew install pkg-config ffmpeg
                                        # Debian: apt-get install pkg-config libavformat-dev
```

## Test

```bash
cargo clippy --all-targets -- -D warnings
cargo test
cargo fmt --check
```

## External tools

| Tool         | Required | Notes |
|--------------|----------|-------|
| `ffmpeg`     | yes      | always |
| `ffprobe`    | yes      | always |
| `nvidia-smi` | not needed | NVENC capability is probed with ffmpeg itself (1-frame test encode); `nvidia-smi` is never called |

## Conventions

- Library errors: `thiserror`. CLI errors: `anyhow`. `EncodeError` is `#[non_exhaustive]`.
- `EncoderCache` is passed by `&` -- never cloned.
- `-r <fps>` is an FFmpeg output option: `build_ffmpeg_command` must place it after `-i`, before the output path.
- Phase 2 (this commit) covers workspace + base data structures. Subsequent phases per `docs/design.md` §7.

## TODO

- (none open)

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
   - `cargo clippy --workspace --all-targets` — are there lints?
   - `cargo test --workspace` — is the workspace green?

5. **When stuck, diagnose the path first.** If a file edit fails because
   "file not found", `cat` the path or run `git ls-files | grep <name>`
   to confirm what exists. Don't guess.

6. **Backwards compat bridges are fine as a step, not a goal.** Re-exports
   in the old module preserve the existing import surface so the public
   API doesn't change during a refactor. The cleanup of those bridges is
   the final stage of the refactor (here: U16 Stage 5).

<!-- code-review-graph MCP tools -->
## MCP Tools: code-review-graph

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
code-review-graph MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `semantic_search_nodes` or `query_graph` instead of Grep
- **Understanding impact**: `get_impact_radius` instead of manually tracing imports
- **Code review**: `detect_changes` + `get_review_context` instead of reading entire files
- **Finding relationships**: `query_graph` with callers_of/callees_of/imports_of/tests_for
- **Architecture questions**: `get_architecture_overview` + `list_communities`

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### Key Tools

| Tool | Use when |
| ------ | ---------- |
| `detect_changes` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context` | Need source snippets for review — token-efficient |
| `get_impact_radius` | Understanding blast radius of a change |
| `get_affected_flows` | Finding which execution paths are impacted |
| `query_graph` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes` | Finding functions/classes by name or keyword |
| `get_architecture_overview` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `detect_changes` for code review.
3. Use `get_affected_flows` to understand impact.
4. Use `query_graph` pattern="tests_for" to check coverage.
