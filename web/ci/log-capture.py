#!/usr/bin/env python3
"""Capture a byte stream without allowing CI output to fill the runner disk."""

from __future__ import annotations

import json
import os
import sys
from collections import deque
from pathlib import Path


HEAD_BYTES = int(os.environ.get("NANOCODEX_CI_LOG_HEAD_BYTES", 32 * 1024 * 1024))
TAIL_BYTES = int(os.environ.get("NANOCODEX_CI_LOG_TAIL_BYTES", 32 * 1024 * 1024))
CHUNK_BYTES = 256 * 1024


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: log-capture.py OUTPUT", file=sys.stderr)
        return 2

    output = Path(sys.argv[1])
    head = bytearray()
    tail: deque[bytes] = deque()
    tail_size = 0
    observed = 0

    while chunk := sys.stdin.buffer.read(CHUNK_BYTES):
        observed += len(chunk)
        head_remaining = HEAD_BYTES - len(head)
        if head_remaining > 0:
            head.extend(chunk[:head_remaining])
            chunk = chunk[head_remaining:]
        if not chunk:
            continue
        tail.append(chunk)
        tail_size += len(chunk)
        while tail_size > TAIL_BYTES:
            overflow = tail_size - TAIL_BYTES
            first = tail[0]
            if len(first) <= overflow:
                tail.popleft()
                tail_size -= len(first)
            else:
                tail[0] = first[overflow:]
                tail_size -= overflow

    truncated = observed > HEAD_BYTES + TAIL_BYTES
    marker = (
        f"\n[... nanocodex CI omitted {observed - HEAD_BYTES - tail_size} log bytes ...]\n"
        .encode()
        if truncated
        else b""
    )
    temporary = output.with_suffix(output.suffix + ".tmp")
    with temporary.open("wb") as target:
        target.write(head)
        target.write(marker)
        for chunk in tail:
            target.write(chunk)
    os.replace(temporary, output)

    metadata = {
        "bytesObserved": observed,
        "bytesStored": output.stat().st_size,
        "truncated": truncated,
    }
    output.with_suffix(output.suffix + ".meta.json").write_text(
        json.dumps(metadata, separators=(",", ":")), encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
