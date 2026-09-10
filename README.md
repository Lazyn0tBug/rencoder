# Rencoder

Smart video transcoder and analyzer. Rust port of `encode.zsh` and
`video_fps_stats.zsh` from the original shell-script toolchain.

## Features

- **Multiple targets**: AV1, H.264, H.265, VP9 (libsvtav1 / libx264 / libx265 / libvpx-vp9)
- **CRF or fixed bitrate** rate control
- **Max FPS cap** with strict comparison (29.97 source at cap=30 stays as-is)
- **NVIDIA acceleration** via `--nv` + `nvidia-smi` (silent fallback when unavailable)
- **Single-shot probe**: one `ffprobe` per file for codec + frame rate
- **Encoder cache**: one `ffmpeg -encoders` invocation per batch
- **Directory or single-file** input (recursive, extension filter)
- **Signal-aware**: partial output is removed on SIGINT
- **Structured logging**: stdout + rotating file sink under `./log/`
- **CLI + library** architecture: the core is reusable without UI dependencies

## Requirements

- **Rust 2021 edition** (stable)
- **FFmpeg** ≥ 6.0 (provides `ffmpeg` and `ffprobe`)
- **Optional**: NVIDIA driver + `nvidia-smi` for hardware acceleration

```sh
# macOS
brew install ffmpeg

# Debian/Ubuntu
sudo apt-get install ffmpeg
```

## Build

```sh
cargo build --release
# Binary: target/release/rencoder
```

### Distribution build

For a packaged release (binary + LICENSE + README bundled into a
versioned tarball plus an extracted copy at a stable path):

```sh
# One-time prereq
cargo install cargo-dist

# Build for the host target
./scripts/dist.sh
# Outputs:
#   dist/rencoder_cli-<target>.tar.xz   versioned archive
#   dist/rencoder_cli-<target>/         extracted: rencoder + LICENSE + README
#   dist/*.sha256                       checksums
```

The script wraps `dist build` (cargo-dist) and stages its output into
`./dist/`. Build for a different target with
`./scripts/dist.sh --target <triple>`. Full multi-target CI builds use
`dist build` directly; see `dist-workspace.toml` for the configured
target list.

## Quickstart

A copyable reference config ships at
[`examples/rencoder.toml.example`](examples/rencoder.toml.example). For
U18, this is a dev-only reference — `rencoder` auto-generates the live
config at `<app_dir>/rencoder.toml` on first run, so most users don't
need to copy anything. Drop the example at `<app_dir>/rencoder.toml`
to override individual fields, or set `$RENENCODER_CONFIG` to point
at any other path.

### Analyze a directory of videos

```sh
rencoder analyze -d ./my_videos/
```

Sample output:

```
24 fps: 2 files
30 fps: 5 files
60 fps: 1 file
Total: 8 files (8 parsed, 0 failed)
```

Output format is set via `[analyze].format` in `rencoder.toml` (`csv`
is the default; `table`, `text`, and `json` are also available).

### Transcode a single file

```sh
rencoder encode -f movie.mp4
# Output: movie.av1.mkv
```

The target format comes from `[encode].format` in `rencoder.toml`
(default `av1`). To switch to H.264, edit the config — no flag needed.

CRF mode (set in config):

```toml
[encode]
format = "av1"
crf = true
bitrate = 45   # CRF value
```

Cap frame rate to 30 fps (60 fps source gets downsampled):

```toml
[encode]
max_fps = 30
```

### Transcode a directory recursively

```toml
[encode]
format = "x264"
bitrate = 5000
extensions = ["mp4", "mov"]
```

```sh
rencoder encode -d ./input/
```

## CLI reference

U17 split: only `target` (`-f`/`-d`) and the action (`encode` /
`analyze`) plus a few per-run overrides (`-i`, `--config`, `-o`,
passthrough args) remain on the CLI. Everything else lives in
`rencoder.toml`.

### `rencoder encode`

```
rencoder encode [OPTIONS] [--] [EXTRA_FFMPEG_ARGS]

Options:
  -f, --file FILE             Single input file
  -d, --dir DIR               Input directory (recursive)
  -i, --interactive           Launch interactive wizard
      --config PATH           Path to TOML config file (default: ./rencoder.toml)
  -o, --output PATH           Per-run data path (.parquet|.jsonl|.csv).
                              Default: ./data/encoding-<UTC-ISO8601>Z.parquet
      --                      All subsequent args pass through to ffmpeg
  -h, --help                  Print help
```

### `rencoder analyze`

```
rencoder analyze [OPTIONS]

Options:
  -d, --dir DIR               Input directory (recursive, default: cwd)
  -f, --file FILE             Single input file
      --config PATH           Path to TOML config file (default: ./rencoder.toml)
  -o, --output PATH           Output file path; `-` for stdout (default)
  -h, --help                  Print help
```

### Global

```
      --version               Print version
  -h, --help                  Print help (top-level)
```

## Config schema (TOML)

```toml
[encode]
format = "av1"             # x264 | hevc | vp9 | av1
bitrate = 5000             # Kbps (default mode)
crf = false                # use CRF; `bitrate` then becomes the CRF value
max_fps = 0                # 0 disables; else one of 24, 25, 30, 60
nv = false                 # enable NVIDIA hardware acceleration
extensions = ["mp4", "mov", "mkv"]   # extension filter (empty = all)

[encode.encoders]
av1 = "libsvtav1"          # override the AV1 encoder (optional)
x264 = "libx264"           # ...
hevc = "libx265"
vp9 = "libvpx-vp9"

[analyze]
format = "csv"             # csv | table | text | json
parallel = false           # parallel analysis via rayon
extensions = []            # extension filter (empty = all)
output = "fps_stats.txt"   # optional; CLI `-o` overrides per-run
```

| Section / field | Type | Constraint | Effect |
|---|---|---|---|
| `[encode].format` | string | one of `x264`, `hevc`, `vp9`, `av1` | target codec; default `av1` |
| `[encode].bitrate` | integer | (optional) | Kbps in bitrate mode, CRF value in CRF mode; default 5000/28 |
| `[encode].crf` | bool | (optional) | use CRF rate control instead of bitrate |
| `[encode].max_fps` | integer | one of `0`, `24`, `25`, `30`, `60` | `0` disables capping; others enforce strict-`>` with epsilon `0.05` |
| `[encode].nv` | bool | (optional) | enable NVIDIA acceleration; falls back silently when unavailable |
| `[encode].extensions` | array of strings | (optional) | file extension filter; empty = all |
| `[encode].encoders.<format>` | string | (optional) | per-format encoder override; missing keys fall back to the built-in default (libsvtav1, libx264, libx265, libvpx-vp9) |
| `[analyze].format` | string | one of `csv`, `table`, `text`, `json` | output format; default `csv` |
| `[analyze].parallel` | bool | (optional) | parallel analysis via rayon |
| `[analyze].extensions` | array of strings | (optional) | file extension filter; empty = all |
| `[analyze].output` | path | (optional) | write analyze output here; CLI `-o` overrides per-run |

A missing config file is equivalent to all defaults; only present fields
override the built-ins. Unknown keys are rejected at load time so typos
surface immediately. Per-format encoder overrides are tried first; if
the named encoder isn't compiled into ffmpeg, the selection chain falls
through to the default.

## Logging

The CLI emits tracing events with structured span context. Two sinks:

1. **stdout** — formatted per tracing-subscriber defaults; `RUST_LOG` controls the level (default `info`).
2. **Rolling file** — `./log/rencoder.YYYY-MM-DD.NN.log`:
   - Daily rollover (NN resets at midnight).
   - Size rollover when the current file reaches 10 MB (NN increments).
   - Files older than 7 days deleted at startup; `RUST_LOG_KEEP=0` keeps everything.

Each event carries the span context (function name + entered spans)
because `encode_cmd` / `analyze_cmd` use `#[instrument]`. The library's
`log` calls are forwarded into the same subscriber via `tracing-log`.

Examples:

```sh
# Verbose analyze
RUST_LOG=debug rencoder analyze -d ./my_videos/

# Encode with spans visible
rencoder encode -d ./input/ -c av1 --max-fps 30
```

## Development

```sh
cargo test                    # 185+ tests
cargo build --release         # release binary
cargo clippy --all-targets     # lints
cargo fmt --check             # formatting
```

Run a single crate's tests:

```sh
cargo test -p rencoder_lib           # core
cargo test -p rencoder_cli --bin rencoder  # CLI unit tests
cargo test --workspace                   # everything
```

## Architecture

See `docs/design.md` for the full design (architecture, KTDs, R-IDs,
test matrix).

- `rencoder_lib/` — core library. All business logic.
- `rencoder_cli/` — CLI binary (clap + tracing + signal handling).
- The library uses `log`; the CLI uses `tracing` with a `tracing-log`
  bridge so both feed the same subscriber.

## License

MIT.
