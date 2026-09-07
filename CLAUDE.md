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
