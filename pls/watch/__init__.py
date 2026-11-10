"""Request log for the router. Delete this package and the router still runs.

One JSON object per line at <profile>/pls/watch/traffic.jsonl, mode 0600.
Rows older than 7 days are dropped, and the file keeps the newest 2000 lines.
There is no page.
"""

import json
import os
import time
from collections import deque
from pathlib import Path
from typing import Callable

_directory: Callable[[], Path] | None = None
_KEEP_SECONDS = 7 * 24 * 60 * 60
_KEEP_LINES = 2000
_PRUNE_EVERY = 64
_since_prune = 0


def start(directory: Callable[[], Path]) -> None:
    global _directory
    _directory = directory


def record(event: dict) -> None:
    global _since_prune
    if _directory is None:
        return
    folder = _directory()
    folder.mkdir(parents=True, exist_ok=True)
    os.chmod(folder, 0o700)
    path = folder / "traffic.jsonl"
    _since_prune += 1
    bulky = path.is_file() and path.stat().st_size > 1_500_000
    if _since_prune >= _PRUNE_EVERY or bulky:
        _drop_old(path)
        _since_prune = 0
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.chmod(path, 0o600)
        with os.fdopen(fd, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(event, ensure_ascii=False) + "\n")
    except Exception:
        pass


def _drop_old(path: Path) -> None:
    if not path.is_file():
        return
    cutoff = time.time() - _KEEP_SECONDS
    kept: deque[str] = deque(maxlen=_KEEP_LINES)
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.rstrip("\n")
            if not line:
                continue
            try:
                event = json.loads(line)
            except Exception:
                continue
            if float(event.get("ts") or 0) >= cutoff:
                kept.append(line)
    tmp = path.with_suffix(".jsonl.tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        if kept:
            handle.write("\n".join(kept) + "\n")
    os.replace(tmp, path)
