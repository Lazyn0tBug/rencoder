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
# Binary: target/release/smart-encode
```

## Quickstart

### Analyze a directory of videos

```sh
smart-encode analyze -d ./my_videos/
```

Sample output:

```
24 fps: 2 files
30 fps: 5 files
60 fps: 1 file
Total: 8 files (8 parsed, 0 failed)
```

Add `-v` to write per-file details to `fps_stats.txt`:

```sh
smart-encode analyze -d ./my_videos/ -v
```

### Transcode a single file

```sh
smart-encode encode -f movie.mp4 -c av1
# Output: movie.av1.mkv
```

CRF mode for AV1 (quality-based):

```sh
smart-encode encode -f movie.mp4 -c av1 --crf -b 45
```

Cap frame rate to 30 fps (60 fps source gets downsampled):

```sh
smart-encode encode -d ./highfps/ -c av1 --max-fps 30
```

### Transcode a directory recursively

```sh
smart-encode encode -d ./input/ -c x264 -b 5000 -e mp4 -e mov
```

## CLI reference

### `smart-encode encode`

```
smart-encode encode [OPTIONS] [--] [EXTRA_FFMPEG_ARGS]

Options:
  -c, --format FORMAT         Target format: x264|hevc|vp9|av1 (default: av1)
  -b, --bitrate K             Bitrate in Kbps, or CRF value with --crf
  -f, --file FILE             Single input file
  -d, --dir DIR               Input directory (recursive)
  -e, --ext EXT               Extension filter (repeatable, default: all)
  -i, --interactive           Launch interactive wizard (Phase 12)
      --nv                    Enable NVIDIA hardware acceleration
      --max-fps FPS           Max FPS cap: 24 | 25 | 30 | 60 | 0 (= disabled, default)
      --crf                   Use CRF mode (AV1 only)
      --config PATH           Path to JSON config file
      --                      All subsequent args pass through to ffmpeg
  -h, --help                  Print help
```

### `smart-encode analyze`

```
smart-encode analyze [OPTIONS]

Options:
  -d, --dir DIR               Input directory (recursive, default: cwd)
  -f, --file FILE             Single input file
  -e, --ext EXT               Extension filter (repeatable)
  -p, --parallel              Parallel analysis (rayon; lands in Phase 12)
  -v, --verbose               Write per-file details to -o file
  -o, --output PATH           Output file for -v (default: fps_stats.txt)
  -h, --help                  Print help
```

### Global

```
      --version               Print version
  -h, --help                  Print help (top-level)
```

## Config schema (JSON)

```json
{
  "max_fps": 30,
  "max_fps_eps": 0.05,
  "encoders": {
    "av1": "libsvtav1",
    "x264": "libx264",
    "hevc": "libx265",
    "vp9": "libvpx-vp9"
  }
}
```

| Field | Type | Constraint | Effect |
|---|---|---|---|
| `max_fps` | number | one of `0`, `24`, `25`, `30`, `60` | `0` disables capping; others enforce strict-`>` with epsilon `max_fps_eps` |
| `max_fps_eps` | number | (optional, default `0.05`) | tolerance band around `max_fps` for the strict comparison |
| `encoders` | object | (optional) | per-format override; missing keys fall back to the built-in default (libsvtav1, libx264, libx265, libvpx-vp9) |
| `encoders.av1` | string | (optional) | override the AV1 encoder (e.g. `libaom-av1`, `av1_nvenc`) |

A missing config file is equivalent to all defaults; only present fields
override the built-ins. Per-format encoder overrides are tried first; if
the named encoder isn't compiled into ffmpeg, falls back to the default
for that format.

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
RUST_LOG=debug smart-encode analyze -d ./my_videos/

# Encode with spans visible
smart-encode encode -d ./input/ -c av1 --max-fps 30
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
cargo test -p smart_encode_lib           # core
cargo test -p smart_encode_cli --bin smart-encode  # CLI unit tests
cargo test --workspace                   # everything
```

## Architecture

See `docs/smart_code.md` for the full design (architecture, KTDs, R-IDs,
test matrix).

- `smart_encode_lib/` — core library. All business logic.
- `smart_encode_cli/` — CLI binary (clap + tracing + signal handling).
- The library uses `log`; the CLI uses `tracing` with a `tracing-log`
  bridge so both feed the same subscriber.

## License

MIT.
