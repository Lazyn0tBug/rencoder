#!/bin/sh
# Build a portable directory: executable and config always stay together.
set -eu
cd "$(dirname "$0")/.."
TARGET=$(rustc -vV | sed -n 's|host: ||p')
if [ "${1:-}" = "--target" ]; then
    TARGET=${2:?missing target triple}
    shift 2
fi
[ -n "$TARGET" ] || { echo "cannot detect Rust target" >&2; exit 1; }
# Release is always used; retain compatibility with the old --release invocation.
if [ "${1:-}" = "--release" ]; then shift; fi
cargo build --locked --release --package rencoder_cli --target "$TARGET" "$@"
python3 scripts/package.py --target "$TARGET"
