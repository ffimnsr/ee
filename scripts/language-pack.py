#!/usr/bin/env python3
"""Build a prebuilt ee runtime grammar pack.

The pack is a deterministic tar.gz containing `manifest.json` followed by the
grammar shared libraries from a built runtime tree. Release CI attaches one pack
per target so `ee do runtime install <language>` can install grammars without a
C compiler, Rust toolchain, or grammar source checkout.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import sys
import tarfile
from pathlib import Path

SCHEMA_VERSION = 1
MANIFEST_NAME = "manifest.json"
GRAMMAR_DIR = "grammars"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def grammar_files(runtime_root: Path) -> list[dict[str, object]]:
    grammar_dir = runtime_root / GRAMMAR_DIR
    if not grammar_dir.is_dir():
        raise SystemExit(f"error: grammar directory not found: {grammar_dir}")

    files: list[dict[str, object]] = []
    for path in sorted(grammar_dir.iterdir()):
        if not path.is_file() or path.is_symlink() or path.name.startswith("."):
            continue
        files.append(
            {
                "path": f"{GRAMMAR_DIR}/{path.name}",
                "sha256": sha256_file(path),
                "size": path.stat().st_size,
            }
        )

    if not files:
        raise SystemExit(f"error: no grammar libraries found in {grammar_dir}")
    return files


def build(args: argparse.Namespace) -> None:
    runtime_root = Path(args.runtime_root)
    output = Path(args.output)
    files = grammar_files(runtime_root)
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "package_version": args.version,
        "target": args.target,
        "files": files,
    }
    manifest_bytes = (
        json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"
    )

    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                add_bytes(archive, MANIFEST_NAME, manifest_bytes)
                for entry in files:
                    path = runtime_root / str(entry["path"])
                    add_file(archive, str(entry["path"]), path)

    print(f"{output} ({len(files)} grammars, {output.stat().st_size} bytes)")


def add_bytes(archive: tarfile.TarFile, name: str, data: bytes) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = 0o644
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    archive.addfile(info, io.BytesIO(data))


def add_file(archive: tarfile.TarFile, name: str, path: Path) -> None:
    info = tarfile.TarInfo(name)
    info.size = path.stat().st_size
    info.mode = 0o644
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    with path.open("rb") as handle:
        archive.addfile(info, handle)


def verify(args: argparse.Namespace) -> None:
    pack = Path(args.pack)
    with tarfile.open(pack, mode="r:gz") as archive:
        manifest_member = archive.getmember(MANIFEST_NAME)
        manifest = json.loads(archive.extractfile(manifest_member).read().decode("utf-8"))
        if manifest.get("schema_version") != SCHEMA_VERSION:
            raise SystemExit("error: unsupported manifest schema_version")
        if args.version and manifest.get("package_version") != args.version:
            raise SystemExit("error: package_version mismatch")
        if args.target and manifest.get("target") != args.target:
            raise SystemExit("error: target mismatch")

        for entry in manifest.get("files", []):
            member = archive.getmember(str(entry["path"]))
            data = archive.extractfile(member).read()
            if len(data) != entry["size"]:
                raise SystemExit(f"error: size mismatch for {entry['path']}")
            if hashlib.sha256(data).hexdigest() != entry["sha256"]:
                raise SystemExit(f"error: sha256 mismatch for {entry['path']}")

    print(f"{pack} verified ({len(manifest.get('files', []))} grammars)")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    build_parser = subparsers.add_parser("build", help="build a language pack")
    build_parser.add_argument("--runtime-root", required=True)
    build_parser.add_argument("--output", required=True)
    build_parser.add_argument("--version", required=True)
    build_parser.add_argument("--target", required=True)
    build_parser.set_defaults(handler=build)

    verify_parser = subparsers.add_parser("verify", help="verify a language pack")
    verify_parser.add_argument("--pack", required=True)
    verify_parser.add_argument("--version")
    verify_parser.add_argument("--target")
    verify_parser.set_defaults(handler=verify)

    args = parser.parse_args()
    args.handler(args)


if __name__ == "__main__":
    try:
        main()
    except (BrokenPipeError, KeyboardInterrupt):
        sys.exit(1)
