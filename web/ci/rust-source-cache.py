#!/usr/bin/env python3
"""Preserve Cargo's incremental graph across source archive overlays."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import time
from pathlib import Path
from typing import Iterable


MANIFEST_VERSION = 1
EXPLICIT_FILES = (
    "Cargo.toml",
    "Cargo.lock",
    "js/bindings/Cargo.toml",
    "py/bindings/Cargo.toml",
)
RECURSIVE_ROOTS = (
    ".cargo",
    "bin",
    "crates",
    "js/bindings/src",
    "py/bindings/src",
)


def regular_files(directory: Path) -> Iterable[Path]:
    if not directory.is_dir():
        return
    for current, directories, filenames in os.walk(directory, followlinks=False):
        directories.sort()
        filenames.sort()
        current_path = Path(current)
        for filename in filenames:
            path = current_path / filename
            if stat.S_ISREG(path.lstat().st_mode):
                yield path


def source_files(workspace: Path) -> list[Path]:
    files: set[Path] = set()
    for relative in EXPLICIT_FILES:
        path = workspace / relative
        if path.exists() and stat.S_ISREG(path.lstat().st_mode):
            files.add(path)
    for relative in RECURSIVE_ROOTS:
        files.update(regular_files(workspace / relative))
    for path in regular_files(workspace / "examples"):
        if path.name == "Cargo.toml" or path.suffix == ".rs":
            files.add(path)
    return sorted(files, key=lambda path: path.relative_to(workspace).as_posix())


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def fingerprints(workspace: Path) -> dict[str, str]:
    return {
        path.relative_to(workspace).as_posix(): sha256(path)
        for path in source_files(workspace)
    }


def snapshot(workspace: Path, manifest: Path) -> None:
    payload = {
        "version": MANIFEST_VERSION,
        "files": fingerprints(workspace),
    }
    temporary = manifest.with_name(f"{manifest.name}.tmp")
    temporary.write_text(
        json.dumps(payload, sort_keys=True, separators=(",", ":")),
        encoding="utf-8",
    )
    temporary.replace(manifest)
    print(f"rust source cache: recorded {len(payload['files'])} inputs")


def previous_fingerprints(manifest: Path) -> dict[str, str]:
    try:
        payload = json.loads(manifest.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return {}
    if not isinstance(payload, dict) or payload.get("version") != MANIFEST_VERSION:
        return {}
    files = payload.get("files")
    if not isinstance(files, dict):
        return {}
    if not all(isinstance(path, str) and isinstance(digest, str) for path, digest in files.items()):
        return {}
    return files


def refresh(workspace: Path, manifest: Path) -> None:
    try:
        reference_ns = manifest.stat().st_mtime_ns
    except FileNotFoundError:
        reference_ns = 0
    previous = previous_fingerprints(manifest)
    current = fingerprints(workspace)
    now_ns = time.time_ns()
    unchanged = 0
    changed = 0
    for relative, digest in current.items():
        path = workspace / relative
        if reference_ns > 0 and previous.get(relative) == digest:
            os.utime(path, ns=(reference_ns, reference_ns), follow_symlinks=False)
            unchanged += 1
        else:
            os.utime(path, ns=(now_ns, now_ns), follow_symlinks=False)
            changed += 1
    manifest.unlink(missing_ok=True)
    print(
        f"rust source cache: retained {unchanged} inputs; invalidated {changed} inputs"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("operation", choices=("snapshot", "refresh"))
    parser.add_argument("workspace", type=Path)
    parser.add_argument("manifest", type=Path)
    arguments = parser.parse_args()
    workspace = arguments.workspace.resolve()
    manifest = arguments.manifest.resolve()
    if arguments.operation == "snapshot":
        snapshot(workspace, manifest)
    else:
        refresh(workspace, manifest)


if __name__ == "__main__":
    main()
