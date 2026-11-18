#!/usr/bin/env python3
"""Package a built CLI with its adjacent config; hash the final archive."""
import argparse
import hashlib
from pathlib import Path
import shutil
import tarfile
import tempfile
import zipfile


def package(root, target):
    name = f"rencoder_cli-{target}"
    binary = "rencoder.exe" if "windows" in target else "rencoder"
    source = root / "target" / target / "release" / binary
    if not source.is_file():
        raise FileNotFoundError(source)
    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    # Build in a fresh temporary directory, never reuse previous package contents.
    with tempfile.TemporaryDirectory(prefix="rencoder-dist-") as temp:
        app = Path(temp) / name
        app.mkdir()
        shutil.copy2(source, app / binary)
        (app / binary).chmod(0o755)
        shutil.copy2(root / "examples/rencoder.toml.example", app / "rencoder.toml")
        (app / "rencoder.toml").chmod(0o644)
        for filename in ("LICENSE", "README.md"):
            shutil.copy2(root / filename, app / filename)
        for directory in ("log", "data"):
            (app / directory).mkdir()
        if "windows" in target:
            archive = dist / f"{name}.zip"
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
                for path in sorted(app.rglob("*")):
                    output.write(path, path.relative_to(app.parent).as_posix())
        else:
            archive = dist / f"{name}.tar.xz"
            with tarfile.open(archive, "w:xz") as output:
                output.add(app, arcname=name)
        # Also provide an immediately runnable host directory.
        unpacked = dist / name
        if unpacked.exists():
            shutil.rmtree(unpacked)
        shutil.copytree(app, unpacked)
    with archive.open("rb") as stream:
        checksum = hashlib.file_digest(stream, "sha256").hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{checksum}  {archive.name}\n", encoding="utf-8")
    print(archive)
    return archive


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    args = parser.parse_args()
    package(Path(__file__).resolve().parent.parent, args.target)
