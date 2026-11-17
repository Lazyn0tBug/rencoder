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
"""

import shutil
import sys
import tempfile
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CONFIG_TEMPLATE = REPO_ROOT / "examples" / "rencoder.toml.example"
KEEP_DIRS = ("log", "data")


def top_level_prefix(names):
    """Return the single top-level directory the archive stores."""
    prefixes = {name.split("/", 1)[0] for name in names if "/" in name}
    if len(prefixes) != 1:
        raise SystemExit(
            f"expected exactly one top-level directory, found {sorted(prefixes)}"
        )
    return prefixes.pop()


def augment(archive: Path) -> None:
    with zipfile.ZipFile(archive) as source:
        names = source.namelist()
        prefix = top_level_prefix(names)
        payload = {name: source.read(name) for name in names}

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
