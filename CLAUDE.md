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
├── rencoder                  # binary (thin: only its own code)
├── librencoder_lib.dylib     # the lib, as a shared library (Windows: rencoder_lib.dll)
├── libstd.dylib              # the Rust runtime, shipped with it
├── rencoder.toml             # config (auto-created on first run if missing)
├── log/                      # runtime logs
└── data/                     # runtime data (Arrow IPC records, etc.)
```

- All persistent state lives in `<app_dir>`; CWD is reserved for input files only.
- `$RENENCODER_CONFIG` / `$RENENCODER_LOG_DIR` / `$RENENCODER_DATA_DIR` override defaults.
- dist (via `scripts/dist.sh`) mirrors this layout — extract a tarball, get a ready-to-run app dir.
- **The lib is a shared library** (design doc §8 row 116): the executables stay thin
  and everything the lib provides — including the data stack — lives in the
  library. The Rust runtime ships with it, so the app never depends on a Rust
  toolchain being installed. CI builds the library and both executables from one
  commit, so the ABI always matches; macOS signing covers the libraries too.

## App-level config (U18)

Only `target` (`-f`/`-d`) and `action` (`encode`/`analyze`) plus per-run
overrides (`-i`, `--config`, `-o`, passthrough args) stay on the CLI --
`-i` (the interactive wizard) is **not implemented**: it says so instead
of pretending.
Everything else (format, bitrate, max-fps, crf, nv, extensions, encoder
overrides, analyze format, parallel) lives in `rencoder.toml` at the
app-level location (binary dir by default; `$RENENCODER_CONFIG` overrides).
See `rencoder_lib::config::app::AppConfig` for the schema and
`examples/rencoder.toml.example` for a dev-only reference. Unknown TOML keys are
rejected so typos surface immediately.

CRF is a `u32` (default 45, sentinel 0 = bitrate mode), not a bool — this
aligns 1:1 with ffmpeg's `-crf N`.

**Logging is configuration too**: the `[log]` section of `rencoder.toml`
(`level` / `max_bytes` / `max_days`) — **no `RUST_LOG` environment
variable**. The level is a *runtime* setting, so a release build logs
exactly as much as a debug build when the config says so (design doc
§3.10.5). Both frontends write the same log file (`<app_dir>/log/`);
the file sink lives in the lib (`rencoder_lib::logging::file_sink`).

## Cross-platform encapsulation (U18)

Platform-specific behavior follows the **"cfg at definition, uniform
signature at call"** principle:

- `#[cfg(...)]` lives only inside the function body that does the
  platform-specific work.
- Callers see one stable signature; no cfg at the call site.
- **Unify the functional semantics, not the mechanism.** The OS
  mechanism differs (Unix `flock` vs Windows share mode) and must not
  be "unified"; what is unified is the *contract* the application
  depends on — same behavior, same assertions on both platforms.
- The library, the CLI **and the GUI** build and test on macOS,
  Linux and Windows. The CI (`.github/workflows/ci.yml`) runs the
  `core` and `ui` gates on all three hosts — see `docs/TESTING.md`.
  The GUI's tests are headless, so CI proves "it compiles and the
  tests pass", not "the window opens" (that one is manual).
- **WebView2 is the system's job**: Windows 11 ships it, Windows 10
  pulls wry's Evergreen bootstrapper once. Shipping a fixed-version
  runtime would add 150MB to every archive.
- Adding a new platform later = edit one file (the definition site).

Concrete: `rencoder_lib::platform` is the **only** file in the repo
with `#[cfg]` in product code (plus `rencoder_cli::signal`, the
registered CLI exception). It holds four contracts:

| Contract | Unix | Windows |
|----------|------|---------|
| `perms::set_config_file_mode` / `set_app_dir_mode` | `0o644` / `0o755` | no-op — ACL inheritance is equivalent |
| `lock::claim` / `lock::probe` (the single-instance guard) | std `flock` (advisory) | open share mode (deny writers, allow readers) |
| `fs_semantics::fold_name` / `same_name` | one folding rule (case-insensitive) | same rule — not split per platform |
| `process::isolate_from_console_events` | no-op (a signal hits one process) | `CREATE_NEW_PROCESS_GROUP` for the encoder |

The lock's contract is the five properties of design doc §3.11.1.1
(atomic takeover, release on process death, probeable without
interfering, **does not block reads**, located by path). `platform::lock`
holds **one platform-agnostic contract test** for them, so both CI
hosts assert the same behavior.

Exception (CLI only): `rencoder_cli::signal` has a Windows branch --
Unix uses `signal-hook` (SIGINT/SIGTERM/SIGHUP), Windows uses
`SetConsoleCtrlHandler` (console events). The divergence stays in that
one file; `install_handler()` keeps one signature everywhere. The
Windows path is compiled and tested by the CI's Windows job.

`rencoder_cli::keys` is the same kind of exception: the **quit key**
(Ctrl-D, a deliberate quit -- exit code 0, unlike Ctrl-C's
interrupt -- exit code 130). Unix reads the terminal in non-canonical
mode (`libc` termios), Windows reads console key events
(`ReadConsoleInput`). It is installed around the batch only, and the
terminal is put back when the batch ends, on `process::exit` and on a
panic.

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
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo fmt --check
```

Running the suite on another host: `docs/TESTING.md` (what each host
needs, what differs per platform, what the CI covers).

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
- The design doc's §8 table tracks what is left; §3.11 is the execution contract.

## Review checklist

The rules live in `docs/design.md` §3.10; these are the checks to run *before*
saying a change is done.

- [ ] **Requirement first.** A new requirement, a new shared state, or a change
      to the concurrency structure gets written into `docs/design.md` **before**
      the code (the RwLock episode: nobody could tell whether it was right
      because the requirement was never written down).
- [ ] **Shared state answered.** For every new shared state: how many readers,
      how many writers, how long is the lock held. If the answer is "one owner",
      it is a plain variable, not a lock (design doc §3.10.1).
- [ ] **Concurrency structure updated.** Changing the actor/worker structure
      means updating §3.10.1's shared-state inventory and thread list in the
      same commit.
- [ ] **Parameter bags.** A function with more than ~5 parameters is asked
      "is this a context object?" (design doc §3.10.4). The test: a group
      becomes an object only if it appears in >= 3 signatures, has exactly one
      owner, and is not mutated in more than one place.
- [ ] **No shadow implementations.** A test double must not re-implement the
      thing it tests -- it replaces one step and drives the same code
      (design doc §3.10.4 rule 2). A duplicated actor let a real bug
      ("cancel never reached the encoder") pass with green tests.
- [ ] **Every `✅` in the doc points at code.** A design-doc row marked done
      must name something that exists. `Event::ActorExited` was specced, marked
      done, and never implemented.
- [ ] **Benign outcomes do not travel on fatal channels.** A rejected command
      is not an error: `Event::Error` is fatal-only and the UI finalizes the
      running batch when it sees one.

## TODO

- (none open)

## Batch check, batch fix, batch verify

A change that spans several files is one change, not several. The unit of
integration is the **task**: a coherent piece of work that can be
implemented, checked and verified on its own.

**Outer loop (the whole job):**

1. **Spread out the whole picture and read it once.** Before planning,
   read everything the work touches in one pass (the files, the call
   sites, the tests). Reading piecemeal while editing is what produces a
   plan built on half the picture.
2. **Decompose into tasks.** Each task has one intent, one verification,
   and a file list you can name up front. The task list is the iteration
   order.
3. **Run each task's inner loop**, in order. Tasks are independent: one
   task's loop ends green before the next one starts.
4. **Integrate.** After the last task, run one more whole check -> fix ->
   verify over everything. If that surfaces problems too big to fix in
   place, decompose them into new tasks and run their loops.

**Inner loop (per task):**

1. **Implement the whole task in one pass.** Write all of it before
   compiling. The compiler tells you what you got wrong; it does not
   tell you what to write.
2. **Check, fix, verify -- then repeat.** One check that surfaces every
   error of this task at once: `cargo check --workspace --all-targets`
   (not `cargo check -p <crate>`, which hides the errors in the crates
   you did not name, and not a per-file loop). Then one fix pass that
   applies every fix in one go. Then the gate (`cargo clippy --workspace
   --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo fmt --check`). Iterate check -> fix -> verify until the task
   is green.

The discipline: **inside a task, the errors are handled in batches and
the iteration happens between the batches.** A systematic mistake (a
renamed field, a changed signature) shows up as N errors at N sites;
fixing them one at a time costs N compiles, and each compile reveals
only the next error, never the shape of the change.

**A check narrower than the task hides errors.** The check must cover the
task's full blast radius (every crate the task touches), otherwise the
errors outside it surface in the next round -- and a task that ends with
errors outside it is not green.

**Granularity is a trade-off.** Too coarse and the task stops being one
unit: the fix pass becomes unrelated edits, and the verification can no
longer attribute anything to what it proved. Too fine and the compile
overhead per task dominates, so batching buys nothing, and the
integration point moves so often that nothing is ever verified as a
whole. If you cannot name a task's file list, the task is not scoped
yet: scope it first (step 1 of the methodology below), then batch it.

State the batch before running it: what this task implements, what it
checks, what it fixes, what proves it.

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
   - One compile at the END of the change set, not after each file (see
     **Batch check, batch fix, batch verify**). One iteration goal.

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

# context-mode — MANDATORY routing rules

You have context-mode MCP tools available. These rules are NOT optional — they protect your context window from flooding. A single unrouted command can dump 56 KB into context and waste the entire session.

## BLOCKED commands — do NOT attempt these

### curl / wget — BLOCKED
Any Bash command containing `curl` or `wget` is intercepted and replaced with an error message. Do NOT retry.
Instead use:
- `ctx_fetch_and_index(url, source)` to fetch and index web pages
- `ctx_execute(language: "javascript", code: "const r = await fetch(...)")` to run HTTP calls in sandbox

### Inline HTTP — BLOCKED
Any Bash command containing `fetch('http`, `requests.get(`, `requests.post(`, `http.get(`, or `http.request(` is intercepted and replaced with an error message. Do NOT retry with Bash.
Instead use:
- `ctx_execute(language, code)` to run HTTP calls in sandbox — only stdout enters context

### WebFetch — BLOCKED
WebFetch calls are denied entirely. The URL is extracted and you are told to use `ctx_fetch_and_index` instead.
Instead use:
- `ctx_fetch_and_index(url, source)` then `ctx_search(queries)` to query the indexed content

## REDIRECTED tools — use sandbox equivalents

### Bash (>20 lines output)
Bash is ONLY for: `git`, `mkdir`, `rm`, `mv`, `cd`, `ls`, `npm install`, `pip install`, and other short-output commands.
For everything else, use:
- `ctx_batch_execute(commands, queries)` — run multiple commands + search in ONE call
- `ctx_execute(language: "shell", code: "...")` — run in sandbox, only stdout enters context

### Read (for analysis)
If you are reading a file to **Edit** it → Read is correct (Edit needs content in context).
If you are reading to **analyze, explore, or summarize** → use `ctx_execute_file(path, language, code)` instead. Only your printed summary enters context. The raw file content stays in the sandbox.

### Grep (large results)
Grep results can flood context. Use `ctx_execute(language: "shell", code: "grep ...")` to run searches in sandbox. Only your printed summary enters context.

## Tool selection hierarchy

1. **GATHER**: `ctx_batch_execute(commands, queries)` — Primary tool. Runs all commands, auto-indexes output, returns search results. ONE call replaces 30+ individual calls.
2. **FOLLOW-UP**: `ctx_search(queries: ["q1", "q2", ...])` — Query indexed content. Pass ALL questions as array in ONE call.
3. **PROCESSING**: `ctx_execute(language, code)` | `ctx_execute_file(path, language, code)` — Sandbox execution. Only stdout enters context.
4. **WEB**: `ctx_fetch_and_index(url, source)` then `ctx_search(queries)` — Fetch, chunk, index, query. Raw HTML never enters context.
5. **INDEX**: `ctx_index(content, source)` — Store content in FTS5 knowledge base for later search.

## Subagent routing

When spawning subagents (Agent/Task tool), the routing block is automatically injected into their prompt. Bash-type subagents are upgraded to general-purpose so they have access to MCP tools. You do NOT need to manually instruct subagents about context-mode.

## Output constraints

- Keep responses under 500 words.
- Write artifacts (code, configs, PRDs) to FILES — never return them as inline text. Return only: file path + 1-line description.
- When indexing content, use descriptive source labels so others can `ctx_search(source: "label")` later.

## ctx commands

| Command | Action |
|---------|--------|
| `ctx stats` | Call the `ctx_stats` MCP tool and display the full output verbatim |
| `ctx doctor` | Call the `ctx_doctor` MCP tool, run the returned shell command, display as checklist |
| `ctx upgrade` | Call the `ctx_upgrade` MCP tool, run the returned shell command, display as checklist |
