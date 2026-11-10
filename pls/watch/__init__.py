"""Traffic log for the router. Delete this package and the router still runs.

The log file stays on disk, so it can be read after the process exits.
The page and the raw log are open on purpose: no token.
"""

import json
from pathlib import Path
from typing import Callable

from fastapi import FastAPI
from fastapi.responses import HTMLResponse, PlainTextResponse

_directory: Callable[[], Path] | None = None


def attach(app: FastAPI, directory: Callable[[], Path]) -> None:
    global _directory
    _directory = directory

    @app.get("/watch")
    def page():
        html = Path(__file__).with_name("page.html").read_text()
        return HTMLResponse(html)

    @app.get("/watch/log")
    def raw():
        path = directory() / "traffic.jsonl"
        text = path.read_text() if path.is_file() else ""
        return PlainTextResponse(text)


def record(event: dict) -> None:
    if _directory is None:
        return
    folder = _directory()
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / "traffic.jsonl"
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(event, ensure_ascii=False) + "\n")
