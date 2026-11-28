#!/usr/bin/env python3
"""Add the U18 single-directory app layout to a zip package.

The Unix tarballs are augmented in place by `scripts/dist.sh` with
`tar -xJf` / `tar -cJf`. The Windows package is a zip, and `zip` /
`unzip` are not reliably available on the GitHub windows runner --
Python 3 is, on every runner.

Usage:
    python3 scripts/augment_zip.py <archive.zip> [...]

Each archive is rewritten in place with these entries added inside its
top-level package directory:

    <pkg>/rencoder.toml      (copy of examples/rencoder.toml.example)
    <pkg>/log/.gitkeep       (empty dir placeholder)
    <pkg>/data/.gitkeep      (empty dir placeholder)

The `.gitkeep` files matter: zip (like tar) does not store empty
directories, so an app dir without them loses `log/` and `data/`.

The archive layout cargo-dist produces is not the same on every
platform: the Unix tarballs carry a `<pkg>-<triple>/` directory, the
Windows zip puts the files at the archive root. Both are handled
here -- the app dir is a *directory*, so a flat archive gets one
created (named after the archive, like the tarballs), and the result
is the same shape on every platform: extract, and you have an app
dir.
"""

import shutil
import sys
import tempfile
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CONFIG_TEMPLATE = REPO_ROOT / "examples" / "rencoder.toml.example"
KEEP_DIRS = ("log", "data")


def normalize_name(name):
    """The entry name with the separators the zip format wants.

    The spec says forward slashes; an archive written on Windows may
    carry backslashes, and a name without any separator is a file at
    the archive root.
    """
    return name.replace("\\", "/")


def top_level_prefix(names):
    """The single top-level directory the archive stores, if any.

    `None` means the archive is **flat** -- every entry sits at the
    root. That is a legitimate layout (cargo-dist's Windows zip does
    it); the caller then creates the directory.
    """
    prefixes = {name.split("/", 1)[0] for name in names if "/" in name}
    if not prefixes:
        return None
    if len(prefixes) != 1:
        raise SystemExit(
            f"expected exactly one top-level directory, found {sorted(prefixes)}"
        )
    return prefixes.pop()


def augment(archive: Path) -> None:
    with zipfile.ZipFile(archive) as source:
        payload = {
            normalize_name(name): source.read(name) for name in source.namelist()
        }

    prefix = top_level_prefix(payload.keys())
    if prefix is None:
        # A flat archive: the files sit at the root. The app dir is a
        # directory, so create one -- named after the archive, exactly
        # like the Unix tarballs (`rencoder_cli-<triple>/`), so the
        # extracted layout is the same on every platform.
        prefix = archive.stem
    # Whatever was at the root belongs to the app dir: the promise is
    # "extract, and you have exactly one app directory", so a stray
    # root file would break it. (The Unix tarball path in
    # scripts/dist.sh applies the same rule.)
    payload = {
        (name if "/" in name else f"{prefix}/{name}"): data
        for name, data in payload.items()
    }

    payload[f"{prefix}/rencoder.toml"] = CONFIG_TEMPLATE.read_bytes()
    for directory in KEEP_DIRS:
        payload[f"{prefix}/{directory}/.gitkeep"] = b""

    with tempfile.TemporaryDirectory(prefix="rencoder-augment-") as temp:
        staged = Path(temp) / archive.name
        with zipfile.ZipFile(staged, "w", zipfile.ZIP_DEFLATED) as output:
            for name in sorted(payload):
                output.writestr(name, payload[name])
        shutil.move(str(staged), str(archive))

    print(f"augmented {archive} with rencoder.toml + {' + '.join(KEEP_DIRS)}/")


def main(argv):
    if len(argv) < 2:
        raise SystemExit(__doc__)
    for arg in argv[1:]:
        archive = Path(arg)
        if not archive.is_file():
            raise SystemExit(f"not a file: {archive}")
        augment(archive)


if __name__ == "__main__":
    main(sys.argv)
