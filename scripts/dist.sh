#!/bin/sh
# scripts/dist.sh -- build the rencoder CLI into ./dist/ for the host target.
#
# Wraps `dist build` (cargo-dist) and stages its output (which lives in
# `target/distrib/`) into the project-root `dist/` so the binary, LICENSE,
# and README land at the path documented in the README and expected by CI.
#
# U18 (single-directory app, smart_code.md §3.6): every tarball is
# augmented with the app-dir layout (rencoder.toml template + empty log/
# + data/ subdirs) so the extracted binary is ready to run -- no `mkdir`,
# no surprise auto-create file on first invocation.
#
# Usage:
#   ./scripts/dist.sh             # build for the host target only
#   ./scripts/dist.sh --release   # pass-through to dist build
#
# Prereq: cargo-dist is installed (`cargo install cargo-dist`).

set -e

cd "$(dirname "$0")/.."

# Detect the host target so the script does the right thing on each
# machine (aarch64-apple-darwin on M1/M2, x86_64-unknown-linux-gnu on
# Linux, etc.) without hardcoding.
TARGET=$(rustc -vV | sed -n 's|host: ||p')
if [ -z "$TARGET" ]; then
    echo "error: could not detect host target via 'rustc -vV'" >&2
    exit 1
fi

echo "Building for host target: $TARGET"
dist build --target "$TARGET" "$@"

# Stage cargo-dist's output into ./dist/. We replace dist/ wholesale so
# stale archives from previous targets don't linger.
rm -rf dist
mkdir -p dist
for f in target/distrib/*.tar.xz target/distrib/*.tar.gz \
         target/distrib/*.sha256 target/distrib/sha256.sum; do
    [ -f "$f" ] || continue
    cp "$f" dist/
done

# Augment each tarball with the U18 single-directory app layout
# (rencoder.toml template + empty log/ + data/). This matches the
# runtime layout described in docs/smart_code.md §3.6, so the extracted
# binary is ready to run with no further setup.
#
# We re-pack each tarball in place rather than re-deriving from scratch,
# so cargo-dist's SHA256 (covering binary + LICENSE + README) stays
# authoritative for the binary contents. The added files (config +
# dirs) are deterministic and live outside the binary's hash surface.
augmented=0
for tarball in dist/rencoder_cli-*.tar.xz; do
    [ -f "$tarball" ] || continue

    workdir=$(mktemp -d)
    tar -xJf "$tarball" -C "$workdir"

    # cargo-dist lays out: <workdir>/rencoder_cli-<triple>/<files>
    app_dir=$(find "$workdir" -mindepth 1 -maxdepth 1 -type d \
              -name 'rencoder_cli-*' | head -1)
    if [ -z "$app_dir" ]; then
        echo "warn: no rencoder_cli-* dir inside $tarball, skipping augment" >&2
        rm -rf "$workdir"
        continue
    fi

    # Config template (matches DEFAULT_CONFIG_TOML in app.rs; dev-only
    # reference copy stays in examples/rencoder.toml.example).
    cp examples/rencoder.toml.example "$app_dir/rencoder.toml"

    # Empty log/ + data/ subdirs with .gitkeep so they survive the tar
    # roundtrip. Real files appear here on first use (auto-create in
    # the lib, write_records in main).
    mkdir -p "$app_dir/log" "$app_dir/data"
    : > "$app_dir/log/.gitkeep"
    : > "$app_dir/data/.gitkeep"

    # Re-pack in place. `tar -C` switches working dir for the
    # duration of the operation but writes to the path we give it --
    # so the output lands at the absolute path of $tarball.new even
    # though $inner_name is read from $workdir. This avoids the bug
    # where (cd $workdir && tar -cJf $tarball.new ...) tried to write
    # to dist/$tarball.new relative to the tempdir (which doesn't
    # exist there).
    inner_name=$(basename "$app_dir")
    tar -C "$workdir" -cJf "$tarball.new" "$inner_name"
    mv "$tarball.new" "$tarball"
    rm -rf "$workdir"
    augmented=$((augmented + 1))
done

# Also extract the host tarball so ./dist/rencoder_cli-<target>/ is
# directly runnable without unpacking by hand.
host_tarball="dist/rencoder_cli-$TARGET.tar.xz"
if [ -f "$host_tarball" ]; then
    tar -xJf "$host_tarball" -C dist/
    echo "Extracted ./dist/rencoder_cli-$TARGET/ (with U18 app_dir layout)"
fi

echo
echo "Augmented $augmented tarball(s) with rencoder.toml + log/ + data/"
echo
echo "Artifacts in dist/:"
ls -la dist/