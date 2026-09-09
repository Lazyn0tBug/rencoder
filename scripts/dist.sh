#!/bin/sh
# scripts/dist.sh -- build the rencoder CLI into ./dist/ for the host target.
#
# Wraps `dist build` (cargo-dist) and stages its output (which lives in
# `target/distrib/`) into the project-root `dist/` so the binary, LICENSE,
# and README land at the path documented in the README and expected by CI.
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

# Also extract the host tarball so ./dist/rencoder is directly runnable
# without unpacking by hand.
if [ -f "target/distrib/rencoder_cli-$TARGET.tar.xz" ]; then
    tar -xJf "target/distrib/rencoder_cli-$TARGET.tar.xz" -C dist/
    echo "Extracted ./dist/rencoder_cli-$TARGET/rencoder"
fi

echo
echo "Artifacts in dist/:"
ls -la dist/
