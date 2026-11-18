# Rencoder

Smart video transcoder and analyzer. Rust port of `encode.zsh` and
`video_fps_stats.zsh`.

## Features

- **Multiple targets**: AV1, H.264, H.265, VP9 (libsvtav1 / libx264 / libx265 / libvpx-vp9)
- **CRF or bitrate** rate control (CRF default = 45, libsvtav1 medium quality)
- **Max FPS cap** with strict comparison (29.97 source at cap=30 stays as-is)
- **NVIDIA acceleration** via `--nv` + `nvidia-smi` (silent fallback when unavailable)
- **Single-shot probe**: one `ffprobe` per file for codec + frame rate
- **Encoder cache**: one `ffmpeg -encoders` invocation per batch
- **Recursive directory or single-file** input, with extension filter
- **Signal-aware**: partial output is removed on SIGINT
- **Structured logging**: stdout + rotating file sink under `<app_dir>/log/`
- **Single-directory app**: binary + config + log + data all live next to each other

## Requirements

- **Rust 2021 edition** (stable) — for building from source
- **FFmpeg** ≥ 6.0 — runtime dependency (provides `ffmpeg` and `ffprobe`)
- **Optional**: NVIDIA driver + `nvidia-smi` for hardware acceleration

```sh
# macOS
brew install ffmpeg

# Debian / Ubuntu
sudo apt-get install ffmpeg
```

## Install

### Pre-built binary (recommended)

Download the archive for your platform from the [Releases] page. Rencoder
ships as a **self-contained directory**: extract anywhere and run the
binary from that directory.

```sh
# macOS Apple Silicon
tar -xJf rencoder_cli-aarch64-apple-darwin.tar.xz
cd rencoder_cli-aarch64-apple-darwin
./rencoder --help

# Linux x86_64 / Windows (portable zip): same pattern, extract and run
```

On first run, Rencoder auto-creates `rencoder.toml`, `log/`, and `data/`
inside the extracted directory. Subsequent runs use those files as-is.

### Build from source

```sh
git clone https://github.com/.../rencoder
cd rencoder
cargo build --release
# Binary: target/release/rencoder
```

The binary looks for `rencoder.toml` in its parent directory (the
binary's `parent()`). To override: `--config /path/to/file.toml` or
`$RENENCODER_CONFIG`.

## Quickstart

```sh
# Analyze a directory of videos
rencoder analyze -d ./my_videos/
# 24 fps: 2 files
# 30 fps: 5 files
# 60 fps: 1 file
# Total: 8 files (8 parsed, 0 failed)

# Transcode a single file with default config (av1, crf=45)
rencoder encode -f movie.mp4
# Output: <app_dir>/data/encoding-<UTC-timestamp>Z.parquet (work record)
# Output: movie.av1.mkv (the actual transcoded video)
```

That's enough to verify the binary works. For real use, edit
`rencoder.toml` to set format / crf / bitrate / extensions per your
workflow.

## Learn more

- **[`tutorial.md`](tutorial.md)** — operator manual. Detailed install
  options, full config schema, every CLI flag, logging, troubleshooting,
  env var overrides.
- **[`docs/design.md`](docs/design.md)** — architecture contract.
  U18 single-directory app design, cross-platform encapsulation, CRF
  semantics, deployment layout.
- **[`CLAUDE.md`](CLAUDE.md)** — conventions for AI / human contributors
  (build commands, refactor methodology, project layout).

## License

MIT.
### Portable packages and releases

Run `bash scripts/dist.sh` (requires Rust and Python 3.11+). The generated `dist/rencoder_cli-<target>/` contains the executable and `rencoder.toml` side by side; no system application-data directory is used. The final tar.xz or zip includes the same directory and a SHA-256 checksum. Every pushed Git tag triggers a release for macOS, Linux and Windows, plus a macOS disk image. Manual release runs require an existing tag.

### Optional native probe

Default builds use the `ffprobe` executable and do not require pkg-config or FFmpeg development libraries. The native C bridge is opt-in: `cargo build -p rencoder_cli --features rencoder_lib/c-probe`. This mode requires pkg-config plus compatible FFmpeg headers and libraries at build time, and the corresponding shared libraries at runtime. Portable release builds use the default subprocess probe.
