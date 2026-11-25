# Rencoder Tutorial

Operator manual for end users. Covers install, configuration, CLI, logging,
data lifecycle, and troubleshooting.

If you're new to Rencoder, read [`README.md`](README.md) first — it covers
what Rencoder does and how to run a 30-second smoke test. This document is
the deeper dive.

---

## 1. Install

### 1.1 Pre-built binary (recommended)

Pre-built archives are published on the [Releases] page for:

- **macOS Apple Silicon** (`aarch64-apple-darwin`) — `.tar.xz` and `.dmg`
- **Linux x86_64** (`x86_64-unknown-linux-gnu`) — `.tar.xz`
- **Windows x86_64** (`x86_64-pc-windows-msvc`) — `.zip` (portable)

Each archive is a **self-contained single-directory app**: binary + config
template + LICENSE + README (+ `log/` and `data/` placeholders on Unix).

```sh
# macOS
tar -xJf rencoder_cli-aarch64-apple-darwin.tar.xz
cd rencoder_cli-aarch64-apple-darwin
./rencoder --help

# Linux: same pattern (extract tar.xz)
# Windows: unzip the .zip, double-click rencoder.exe or run from cmd
```

On first run, Rencoder auto-creates the config file inside the
extracted directory:

```
<your-install-dir>/
├── rencoder                  # the binary
├── rencoder.toml             # auto-generated starter config (edit this)
├── LICENSE
├── README.md
├── log/                       # auto-created on first log write
└── data/                      # auto-created on first encode write
```

This is the **U18 single-directory app** design — all persistent state
lives next to the binary, no `~/.config/rencoder/`, no `/etc/rencoder/`.

### 1.2 Install via package manager

| Platform | Command |
|----------|---------|
| macOS | `brew install --cask rencoder` (planned; not yet published) |
| Linux (Debian / Ubuntu) | `apt install rencoder` (planned) |
| Windows | `winget install rencoder` (planned) |

For now, use the pre-built archive or build from source.

### 1.3 Build from source

```sh
git clone https://github.com/.../rencoder
cd rencoder
cargo build --release
# Binary at: target/release/rencoder
```

Source build produces the same single-directory layout on first run.
External dependencies: `ffmpeg` and `ffprobe` in `$PATH`.

---

## 2. Configuration

Rencoder reads `rencoder.toml` on every invocation. The file lives in
the **app directory** (the binary's parent directory) by default — no
flags needed for the typical case.

### 2.1 Path resolution

| Path | Default | Override |
|------|---------|----------|
| Config file | `<app_dir>/rencoder.toml` | `--config PATH` or `$RENENCODER_CONFIG` |
| Log directory | `<app_dir>/log/` | `$RENENCODER_LOG_DIR` |
| Data directory | `<app_dir>/data/` | `$RENENCODER_DATA_DIR` |

`<app_dir>` = the binary's parent directory (`current_exe().parent()`).
On macOS this is typically `Contents/MacOS/..` of an `.app` bundle, or
the directory the user extracted the tarball into.

If both an env var override is unset **and** `<app_dir>` cannot be
determined, Rencoder fails fast with a clear error message — it never
silently falls back to `$CWD` or `$HOME`.

### 2.2 First-run auto-create

If `rencoder.toml` doesn't exist at the resolved path on first run,
Rencoder writes a fully-commented starter config, then uses it. Existing
files are **never** overwritten — Rencoder fails with a parse error if
your config is malformed.

```
rencoder: created /Users/me/.local/bin/rencoder.toml
         (no config found; edit to customize)
```

The auto-created file has all schema fields present with their default
values plus inline comments explaining each field. To customize, edit
the file directly — no flag needed to point at a specific config; the
auto-resolve finds it.

### 2.3 Config schema

```toml
[encode]
# Target format. One of: x264 | hevc | vp9 | av1. Default: av1.
format = "av1"

# Bitrate in Kbps (used when crf = 0). Ignored in CRF mode.
# Default: 5000.
bitrate = 5000

# CRF quality value (libsvtav1 scale, 0..=63). Aligns 1:1 with ffmpeg's
# `-crf N`. Sentinel `0` switches to bitrate mode (uses `bitrate` field).
# Default: 45 (libsvtav1 medium quality).
crf = 45

# Cap source FPS at canonical values (0, 24, 25, 30, 60). 0 disables.
# Default: 0.
max_fps = 0

# Enable NVIDIA hardware acceleration via NVENC. Falls back silently
# to software encoding when nvidia-smi is missing or no GPU is detected.
# Default: false.
nv = false

# Extension filter for `encode -d DIR`. Empty list = all files.
extensions = ["mp4", "mov", "mkv"]

[encode.encoders]
# Per-format encoder overrides. Uncomment to use a different encoder
# than the built-in default (libsvtav1, libx264, libx265, libvpx-vp9).
# av1 = "libsvtav1"
# x264 = "libx264"
# hevc = "libx265"
# vp9 = "libvpx-vp9"

[analyze]
# Output format. One of: csv | table | text | json. Default: csv.
format = "csv"

# Parallel analysis via rayon. Default: false.
parallel = false

# Optional: write the analyze output to this path instead of stdout.
# The CLI `-o` flag overrides this per-run; `-` means stdout (default).
# output = "video_stats.csv"
```

### 2.4 CRF semantics

| `crf` value | Mode | Quality / Bitrate |
|-------------|------|-------------------|
| `0` (sentinel) | Bitrate | `bitrate` field, default 5000 Kbps |
| `> 0` | CRF | `crf` value is the libsvtav1 quality (0..=63); `bitrate` ignored |
| field omitted | CRF | Defaults to 45 (serde default fires) |

This aligns 1:1 with `ffmpeg -crf N`. Values outside `0..=63` are rejected
at load time.

### 2.5 Field reference

| Section / field | Type | Default | Effect |
|---|---|---|---|
| `[encode].format` | string (one of `x264`/`hevc`/`vp9`/`av1`) | `av1` | Target codec |
| `[encode].bitrate` | integer (Kbps) | `5000` | Bitrate in bitrate mode |
| `[encode].crf` | integer (0..=63) | `45` | CRF quality; `0` switches to bitrate mode |
| `[encode].max_fps` | integer (one of `0`/`24`/`25`/`30`/`60`) | `0` | Cap source FPS; `0` disables |
| `[encode].nv` | bool | `false` | Enable NVENC; falls back silently |
| `[encode].extensions` | array of strings | `["mp4", "mov", "mkv"]` | File extension filter; empty = all |
| `[encode].encoders.<fmt>` | string | (none) | Per-format encoder override; missing = built-in default |
| `[analyze].format` | string (one of `csv`/`table`/`text`/`json`) | `csv` | Output format |
| `[analyze].parallel` | bool | `false` | Parallel analysis via rayon |
| `[analyze].extensions` | array of strings | (empty = all) | File extension filter |
| `[analyze].output` | path | (none) | Output file path; CLI `-o` overrides per-run |

Unknown keys are rejected at load time — typos surface immediately.

---

## 3. CLI reference

### 3.1 Global

```text
      --version               Print version
  -h, --help                  Print help (top-level or per-subcommand)
```

### 3.2 `rencoder encode`

```text
rencoder encode [OPTIONS] [--] [EXTRA_FFMPEG_ARGS]

Options:
  -f, --file FILE             Single input file
  -d, --dir DIR               Input directory (recursive)
  -i, --interactive           Launch interactive wizard
      --config PATH           Path to TOML config file
                              (default: <app_dir>/rencoder.toml;
                              honors $RENENCODER_CONFIG)
  -o, --output PATH           Per-run data path (.arrow|.jsonl|.csv).
                              Default: <app_dir>/data/encoding-<UTC-ISO8601>Z.arrow
      --                      All subsequent args pass through to ffmpeg
  -h, --help                  Print help
```

Either `-f` or `-d` is required (or use `-i` for interactive mode). On
SIGINT, the in-progress output file is removed before exiting.

### 3.3 `rencoder analyze`

```text
rencoder analyze [OPTIONS]

Options:
  -d, --dir DIR               Input directory (recursive, default: cwd)
  -f, --file FILE             Single input file
      --config PATH           Path to TOML config file
                              (default: <app_dir>/rencoder.toml;
                              honors $RENENCODER_CONFIG)
  -o, --output PATH           Output file path; `-` for stdout (default)
  -h, --help                  Print help
```

The `-o` value is the path to write the detailed analysis to. Without
`-o`, output goes to stdout. The summary line always goes to stderr.

---

## 4. Examples

### 4.1 Analyze

```sh
# Count frame-rate distribution of a directory
rencoder analyze -d ./my_videos/
# 24 fps: 2 files
# 30 fps: 5 files
# 60 fps: 1 file
# Total: 8 files (8 parsed, 0 failed)

# Save detailed CSV
rencoder analyze -d ./my_videos/ -o video_stats.csv

# Analyze a single file
rencoder analyze -f single_movie.mp4

# JSON output via config
# In rencoder.toml: [analyze] format = "json"
rencoder analyze -d ./my_videos/ -o stats.json

# Parallel analysis (CPU-bound; large dirs benefit)
# In rencoder.toml: [analyze] parallel = true
rencoder analyze -d ./my_videos/
```

### 4.2 Transcode

```sh
# Default (av1, crf=45) — single file
rencoder encode -f movie.mp4
# Output: movie.av1.mkv

# Switch to H.264 via config (no flag needed)
# In rencoder.toml: [encode] format = "x264"
rencoder encode -f movie.mp4
# Output: movie.h264.mkv

# Bitrate mode (CRF=0 in config)
# In rencoder.toml:
#   crf = 0
#   bitrate = 8000   # 8 Mbps
rencoder encode -d ./input/

# Cap frame rate to 30 fps (60 fps source gets downsampled)
# In rencoder.toml: [encode] max_fps = 30
rencoder encode -d ./input/ -f 60fps_source.mp4

# Recursive directory encode (x264, 5 Mbps, mp4/mov only)
# In rencoder.toml:
#   [encode]
#   format = "x264"
#   crf = 0
#   bitrate = 5000
#   extensions = ["mp4", "mov"]
rencoder encode -d ./input/
```

### 4.3 Passthrough ffmpeg args

Anything after `--` is forwarded to `ffmpeg` verbatim:

```sh
# Add a custom ffmpeg flag
rencoder encode -f movie.mp4 -- -preset 6

# Multiple flags
rencoder encode -d ./input/ -- -tune film -preset slow
```

---

## 5. Logging

Rencoder emits structured tracing events with span context. Two sinks:

1. **stderr** — formatted per `tracing-subscriber` defaults.
2. **Rolling file** — `<app_dir>/log/rencoder.YYYY-MM-DD.NN.log`:
   - Daily rollover (NN resets at midnight).
   - Size rollover when the current file reaches 10 MB (NN increments).
   - Files older than 7 days deleted at startup.

Each event carries span context (function name + entered spans) because
`encode_cmd` / `analyze_cmd` use `#[instrument]`.

The GUI writes the same log file (its own log pane shows the same
events while the app is open).

Override the log directory via `RENENCODER_LOG_DIR`:

```sh
RENENCODER_LOG_DIR=/tmp/rencoder-debug ./rencoder encode -d ~/Movies
```

### 5.1 Verbosity control

The level is **configuration**, not an environment variable — it lives
in the config file next to the app (`<app_dir>/rencoder.toml`):

```toml
[log]
level = "debug"       # off | error | warn | info | debug | trace
max_bytes = 10485760  # roll the log file at 10 MB
max_days = 7          # keep a week of log files
```

The level is a *runtime* setting: a release build logs exactly as much
as a debug build when the config says so. Change it, restart the app,
read the log — no rebuild, no environment variable.

```sh
# what the app just did
$EDITOR <app_dir>/rencoder.toml   # set level = "debug"
rencoder encode -d ./input/
tail -f <app_dir>/log/rencoder.*.log
```

---

## 6. Data lifecycle

Rencoder writes per-run work records to `<app_dir>/data/`. These are
Arrow IPC/JSONL/CSV files capturing source / process / result / status
for each transcoded file. Use them for batch analytics:

```sh
# List recent runs
ls -lh <app_dir>/data/
# encoding-2026-09-10T15-23-24Z.arrow
# encoding-2026-09-10T15-30-01Z.arrow

# Analyze with pyarrow (the records are Arrow IPC)
python3 -c "
import glob, pyarrow as pa, pyarrow.ipc as ipc
for path in sorted(glob.glob('<app_dir>/data/*.arrow')):
    table = ipc.open_file(pa.memory_map(path)).read_all()
    print(path, table.num_rows, 'rows')
"
```

The `-o` flag overrides the per-run data path (see §3.2). Use this to
write to a different directory for cross-batch analytics.

Override the data directory globally via `RENENCODER_DATA_DIR`:

```sh
# Put data on a different filesystem
RENENCODER_DATA_DIR=/mnt/big/encodings ./rencoder encode -d ~/Movies
```

---

## 7. Troubleshooting

### 7.1 "could not determine config location" error

`<app_dir>` couldn't be detected (`current_exe()` failed). Common causes:

- Binary was deleted between launch and exec (unusual).
- Running from a directory mounted on a filesystem where the kernel
  can't resolve `..` (e.g., some FUSE mounts).
- Running in a sandbox that hides the binary's real path.

Workaround: pass `--config /path/to/rencoder.toml` explicitly, or set
`$RENENCODER_CONFIG`.

### 7.2 "Permission denied" when writing log/ or data/

The binary runs as the user who invoked it; it can't write to
directories owned by another user or to read-only filesystems.

Workaround: redirect with env vars:

```sh
RENENCODER_LOG_DIR=/tmp/logs ./rencoder encode -d ./input/
RENENCODER_DATA_DIR=/tmp/data ./rencoder encode -d ./input/
```

### 7.3 ffmpeg / ffprobe not found

Rencoder shells out to `ffmpeg` and `ffprobe` directly. Install them:

```sh
# macOS
brew install ffmpeg

# Debian / Ubuntu
sudo apt-get install ffmpeg

# Windows: download from https://ffmpeg.org/download.html
```

Rencoder expects FFmpeg ≥ 6.0 (libsvtav1 max CRF = 63 requires this).

### 7.4 NVIDIA acceleration not engaging

`--nv` triggers NVENC, but only if:
- `nvidia-smi` is on `$PATH`
- A GPU is detected
- The bundled ffmpeg was compiled with the corresponding NVENC encoder

If any of these fail, Rencoder silently falls back to software
encoding. Check stderr / logs for `nvidia-smi` output.

### 7.5 First-run config auto-create — what file did it write?

```sh
rencoder: created /Users/me/.local/bin/rencoder.toml
         (no config found; edit to customize)
```

The path is the resolved app_dir + `rencoder.toml`. To override the
config location entirely, pass `--config PATH` or set
`$RENENCODER_CONFIG` before invoking.

---

## 8. Next steps

- **Architecture**: see [`docs/design.md`](docs/design.md) for the U18
  single-directory app design, cross-platform encapsulation, CRF
  semantics, deployment layout, and GitHub Actions workflow.
- **Contributing**: see [`CLAUDE.md`](CLAUDE.md) for build commands,
  refactor methodology, project conventions.
