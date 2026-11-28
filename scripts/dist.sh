#!/bin/sh
# scripts/dist.sh -- build the rencoder CLI and UI into ./dist/ for the host target.
#
# Wraps `dist build` (cargo-dist) and stages its output (which lives in
# `target/distrib/`) into the project-root `dist/` so the binaries, LICENSE,
# and README land at the path documented in the README and expected by CI.
#
# Both `rencoder_cli` (U17) and `rencoder_ui` (U21) are built. Each
# gets its own tarball with a self-contained U18 app_dir (rencoder.toml
# + log/ + data/) so the user picks one to install. We don't merge
# them into a single tarball because that would require re-thinking
# the "single binary per app_dir" invariant in docs/smart_code.md
# §3.6 -- two binaries sharing state is a different deployment shape.
#
# U18 (single-directory app, smart_code.md §3.6): every tarball is
# augmented with the app-dir layout (rencoder.toml template + empty
# log/ + data/ subdirs) so the extracted binary is ready to run --
# no `mkdir`, no surprise auto-create file on first invocation.
#
# Usage:
#   ./scripts/dist.sh                    # build for the host target only
#   ./scripts/dist.sh --target <triple>  # build for an explicit target
#   ./scripts/dist.sh --release          # pass-through to dist build
#
# Every archive staged into ./dist/ is augmented with the U18 app-dir
# layout (rencoder.toml + log/ + data/), and the checksums are
# recomputed afterwards -- cargo-dist hashes the archive before the
# augmentation, so its own checksum files describe the wrong bytes.
#
# Prereq: cargo-dist is installed (`cargo install cargo-dist`).

set -e

cd "$(dirname "$0")/.."

# Detect the host target so the script does the right thing on each
# machine (aarch64-apple-darwin on M1/M2, x86_64-unknown-linux-gnu on
# Linux, etc.) without hardcoding. `--target <triple>` overrides the
# detection -- the CI passes the matrix triple explicitly rather than
# relying on the runner's host triple happening to match.
TARGET=$(rustc -vV | sed -n 's|host: ||p')
if [ "${1:-}" = "--target" ]; then
    TARGET=${2:?missing target triple}
    shift 2
fi
if [ -z "$TARGET" ]; then
    echo "error: could not detect host target via 'rustc -vV'" >&2
    exit 1
fi

echo "Building for host target: $TARGET"
dist build --target "$TARGET" "$@"

# Stage cargo-dist's output into ./dist/. We replace dist/ wholesale so
# stale archives from previous targets don't linger. Glob picks up
# both rencoder_cli-* and rencoder_ui-* (and any future rencoder_*
# binary packages) without per-binary hardcoding.
#
# **One published format** ✅ ✅: cargo-dist 0.32 emits `.tar.xz` for the
# packages and `.tar.gz` only for the *source* archive (which is not
# published). So this script stages, augments, checksums and extracts
# exactly `.tar.xz` (+ `.zip` on Windows). If cargo-dist's format ever
# changes, this script and the workflow's upload list change **together**
# -- a glob that names a format *family* would publish the same package
# twice, or leave a checksum behind for an archive nobody uploaded (the
# release job's own `sha256sum -c` gate would then fail -- docs/CI.md
# CI-19 ✅).
rm -rf dist
mkdir -p dist
for f in target/distrib/*.tar.xz \
         target/distrib/*.zip target/distrib/*.sha256 \
         target/distrib/sha256.sum; do
    [ -f "$f" ] || continue
    # cargo-dist's source archive holds the repo source, not an app
    # package -- there is nothing to install from it.
    case "$(basename "$f")" in
        source.tar.*) continue ;;
    esac
    cp "$f" dist/
done

# Augment each tarball with the U18 single-directory app layout
# (rencoder.toml template + empty log/ + data/). This matches the
# runtime layout described in docs/smart_code.md §3.6, so the
# extracted binary is ready to run with no further setup.
#
# We re-pack each archive in place. cargo-dist's own checksums are
# computed BEFORE the augmentation and therefore describe the
# pre-augmentation archive -- they are recomputed further down.
#
# `rencoder_*-*` glob covers both rencoder_cli-* and rencoder_ui-*
# packages. The matching inner dir (rencoder_cli-* or rencoder_ui-*)
# is detected dynamically rather than per-binary hardcoded. Only
# `.tar.xz` is handled: it is what cargo-dist 0.32 emits for packages
# (see the staging comment above).
augmented=0
for tarball in dist/rencoder_*-*.tar.xz; do
    [ -f "$tarball" ] || continue

    workdir=$(mktemp -d)
    tar -xJf "$tarball" -C "$workdir"

    # The archive has one top-level package directory, or it is flat
    # (the files sit at the root). This is the **same rule**
    # scripts/augment_zip.py applies to the Windows zip: a flat archive
    # gets a directory created, named after the archive, so the
    # extracted layout is the same on every platform.
    #
    # The detection is **structural** -- how many top-level directories
    # are there -- not by name. A name-based rule ("a dir called
    # rencoder_*") treats an archive whose single top directory
    # happened to be named differently as flat and wraps it in a second
    # directory; and it silently drops the entries it did not match.
    top_dirs=$(find "$workdir" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
    if [ "$top_dirs" -gt 1 ]; then
        echo "error: $tarball has $top_dirs top-level directories; expected one" >&2
        rm -rf "$workdir"
        exit 1
    fi
    if [ "$top_dirs" -eq 1 ]; then
        inner_top=$(find "$workdir" -mindepth 1 -maxdepth 1 -type d | head -1)
        # Files at the root belong to the package too: moving them in
        # keeps the app dir self-contained (the old, name-based rule
        # dropped them silently).
        for entry in "$workdir"/* "$workdir"/.[!.]*; do
            [ -e "$entry" ] || continue
            if [ "$entry" != "$inner_top" ] && [ -f "$entry" ]; then
                mv "$entry" "$inner_top"/
            fi
        done
    else
        # A **flat** archive: the files sit at the root. The app dir is
        # a directory, so create one -- named after the archive, the
        # same convention the Windows zip uses (scripts/augment_zip.py)
        # -- so the extracted layout is the same on every platform.
        # Skipping the augmentation here would ship a package that does
        # not match the documented app dir, silently.
        inner_name=$(basename "$tarball" | sed 's/\.tar\.[a-z0-9]*$//')
        inner_top="$workdir/$inner_name"
        mkdir -p "$inner_top"
        for entry in "$workdir"/* "$workdir"/.[!.]*; do
            [ -e "$entry" ] || continue
            if [ "$entry" != "$inner_top" ]; then
                mv "$entry" "$inner_top"/
            fi
        done
    fi

    # Config template (matches DEFAULT_CONFIG_TOML in app.rs; dev-only
    # reference copy stays in examples/rencoder.toml.example).
    cp examples/rencoder.toml.example "$inner_top/rencoder.toml"

    # Empty log/ + data/ subdirs with .gitkeep so they survive the
    # tar roundtrip. Real files appear here on first use (auto-create
    # in the lib, write_records in main).
    mkdir -p "$inner_top/log" "$inner_top/data"
    : > "$inner_top/log/.gitkeep"
    : > "$inner_top/data/.gitkeep"

    # Re-pack in place. `tar -C` switches working dir for the
    # duration of the operation but writes to the path we give it --
    # so the output lands at the absolute path of $tarball.new even
    # though $inner_name is read from $workdir. This avoids the bug
    # where (cd $workdir && tar -cXj $tarball.new ...) tried to
    # write to dist/$tarball.new relative to the tempdir (which
    # doesn't exist there).
    inner_name=$(basename "$inner_top")
    tar -C "$workdir" -cJf "$tarball.new" "$inner_name"
    mv "$tarball.new" "$tarball"
    rm -rf "$workdir"
    augmented=$((augmented + 1))
done

# Augment zip packages (Windows) with the same U18 app-dir layout.
# `zip` / `unzip` are not reliably available on the GitHub windows
# runner, so the zip is rewritten with Python's `zipfile` (Python 3 is
# preinstalled on every runner; `python3` may be missing on Windows,
# hence the fallback to `python`).
PYTHON=python3
command -v python3 >/dev/null 2>&1 || PYTHON=python
if ! command -v "$PYTHON" >/dev/null 2>&1; then
    echo "error: neither python3 nor python found (needed for zip augmentation)" >&2
    exit 1
fi
for zipfile in dist/*.zip; do
    [ -f "$zipfile" ] || continue
    "$PYTHON" scripts/augment_zip.py "$zipfile"
    augmented=$((augmented + 1))
done

# Recompute the checksums AFTER augmentation. The archives changed
# (rencoder.toml + log/ + data/ were added), so cargo-dist's checksums
# -- taken before augmentation -- no longer describe them. The release
# job verifies these files with `sha256sum -c` before publishing.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        echo "error: neither sha256sum nor shasum found" >&2
        exit 1
    fi
}
rm -f dist/*.sha256 dist/sha256.sum
: > dist/sha256.sum
# Only the archives that are published get a checksum file: the
# release job runs `sha256sum -c` over them, and a checksum for an
# archive that was not uploaded makes that gate fail (docs/CI.md
# CI-19).
for archive in dist/*.tar.xz dist/*.zip; do
    [ -f "$archive" ] || continue
    archive_name=$(basename "$archive")
    sum=$(sha256_of "$archive")
    # Bare file name in the checksum file so `sha256sum -c` resolves it
    # against the directory holding the archives.
    printf '%s  %s\n' "$sum" "$archive_name" > "dist/$archive_name.sha256"
    printf '%s  %s\n' "$sum" "$archive_name" >> dist/sha256.sum
done

# Also extract the host package(s) so ./dist/rencoder_<bin>-<target>/
# is directly runnable without unpacking by hand. Iterates over
# both rencoder_cli-$TARGET and rencoder_ui-$TARGET (whichever
# exist).
for bin in rencoder_cli rencoder_ui; do
    host_tarball="dist/${bin}-${TARGET}.tar.xz"
    [ -f "$host_tarball" ] || continue
    tar -xJf "$host_tarball" -C dist/
    echo "Extracted ./dist/${bin}-${TARGET}/ (with U18 app_dir layout)"
done

echo
echo "Augmented $augmented package(s) with rencoder.toml + log/ + data/"
echo "Checksums recomputed after augmentation (dist/*.sha256, dist/sha256.sum)"
echo
echo "Artifacts in dist/:"
ls -la dist/