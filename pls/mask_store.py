"""Per-harness map from mask tokens back to the values that produced them.

One SQLite file per harness. The file never leaves this machine. A token is
``⟦pii:N⟧``; the span category is kept on the span and is not part of the token.
"""

from __future__ import annotations

import os
import re
import sqlite3
import threading
import time
from pathlib import Path

MASK_SAFETY = (
    "Tokens like ⟦pii:1⟧ are personal data kept on this device; "
    "copy them unchanged and they are restored before the subagent runs."
)
TOKEN_RE = re.compile(r"⟦pii:(\d+)⟧")
_HARNESS_RE = re.compile(r"[a-z0-9_]+")
# Raw values are personal data. Drop them a week after they were stored.
RETENTION_S = 7 * 24 * 60 * 60


class MaskError(ValueError):
    """The span batch cannot be applied. The router withholds the text."""


def _profile_root() -> Path:
    base = (
        os.environ.get("WALLY_PROFILE_DIR")
        or os.environ.get("RCLI_PROFILE_DIR")
        or str(Path.home() / ".config" / "wally")
    )
    return Path(base) / "pls" / "masks"


def _offset(span: dict, name: str) -> int:
    value = span.get(name) if isinstance(span, dict) else None
    if isinstance(value, bool) or not isinstance(value, int):
        raise MaskError(f"{name} is not an int")
    return value


class MaskStore:
    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self._lock = threading.Lock()
        self._db = sqlite3.connect(self.path, check_same_thread=False)
        self._db.execute(
            """
            CREATE TABLE IF NOT EXISTS masks (
                session_id TEXT NOT NULL,
                token TEXT NOT NULL,
                value TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, token)
            )
            """
        )
        columns = {row[1] for row in self._db.execute("PRAGMA table_info(masks)")}
        if "created_at" not in columns:
            self._db.execute(
                "ALTER TABLE masks ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0"
            )
        now = int(time.time())
        # A row with no timestamp predates retention. Refreshing it to "now"
        # would keep personal values whose age is unknown.
        self._db.execute("DELETE FROM masks WHERE created_at = 0 OR created_at < ?", (now - RETENTION_S,))
        self._db.execute(
            """
            CREATE TABLE IF NOT EXISTS token_seq (
                session_id TEXT PRIMARY KEY,
                highest INTEGER NOT NULL
            )
            """
        )
        for session_id, token in self._db.execute("SELECT session_id, token FROM masks"):
            match = TOKEN_RE.fullmatch(token)
            if not match:
                continue
            number = int(match.group(1))
            self._db.execute(
                """
                INSERT INTO token_seq (session_id, highest) VALUES (?, ?)
                ON CONFLICT(session_id) DO UPDATE SET
                    highest = MAX(token_seq.highest, excluded.highest)
                """,
                (session_id, number),
            )
        self._db.commit()

    def mask(self, session_id: str, text: str, spans: list[dict]) -> str:
        """Replace spans with tokens. Does not add MASK_SAFETY.

        Each span is {"start": int, "end": int, "category": str}.
        Offsets are Python characters. category is not stored; the token
        map only keeps the value. An empty span list returns text unchanged.
        """
        if not spans:
            if TOKEN_RE.search(text):
                self._reserve_literals(session_id, text)
            return text
        ranges = self._ranges(text, spans)
        with self._lock:
            try:
                self._db.execute("BEGIN IMMEDIATE")
                tokens = [
                    self._token_for(session_id, text[start:end], text) for start, end in ranges
                ]
                self._db.commit()
            except Exception:
                self._db.rollback()
                raise
        out = text
        for (start, end), token in sorted(
            zip(ranges, tokens), key=lambda item: item[0][0], reverse=True
        ):
            out = out[:start] + token + out[end:]
        return out

    def demask(self, session_id: str, text: str) -> str:
        """Replace tokens known in this session. Unknown tokens stay.

        A value is not scanned for tokens after it is put back.
        """
        found = list(TOKEN_RE.finditer(text))
        if not found:
            return text
        with self._lock:
            rows = self._db.execute(
                "SELECT token, value FROM masks WHERE session_id = ?",
                (session_id,),
            ).fetchall()
        known = {token: value for token, value in rows}
        out = text
        for match in reversed(found):
            value = known.get(match.group(0))
            if value is None:
                continue
            out = out[: match.start()] + value + out[match.end() :]
        return out

    def remask(self, session_id: str, text: str) -> str:
        """Put this session's values back behind their tokens.

        Longer values go first, so one value is not cut out of another.
        """
        if not text:
            return text
        with self._lock:
            rows = self._db.execute(
                "SELECT token, value FROM masks WHERE session_id = ?",
                (session_id,),
            ).fetchall()
        pairs = [(token, value) for token, value in rows if value]
        pairs.sort(key=lambda row: len(row[1]), reverse=True)
        if not pairs:
            return text
        lookup = {value: token for token, value in pairs}
        pattern = re.compile(
            r"(?<!\w)(?:" + "|".join(re.escape(value) for _, value in pairs) + r")(?!\w)"
        )
        value_spans = [(match.start(), match.end(), lookup[match.group(0)])
                       for match in pattern.finditer(text)]
        covered = [(start, end) for start, end, _ in value_spans]
        marker = "\ue000"
        while marker in text:
            marker += "\ue001"
        edits: list[tuple[int, int, str | None]] = [
            (start, end, token) for start, end, token in value_spans
        ]
        for match in TOKEN_RE.finditer(text):
            if any(match.start() >= start and match.end() <= end for start, end in covered):
                continue
            edits.append((match.start(), match.end(), None))
        edits.sort()
        pieces: list[str] = []
        held: list[str] = []
        cursor = 0
        for start, end, replacement in edits:
            if start < cursor:
                continue
            pieces.append(text[cursor:start])
            if replacement is None:
                held.append(text[start:end])
                pieces.append(f"{marker}{len(held) - 1}{marker}")
            else:
                pieces.append(replacement)
            cursor = end
        pieces.append(text[cursor:])
        out = "".join(pieces)
        for index, token in enumerate(held):
            out = out.replace(f"{marker}{index}{marker}", token, 1)
        return out

    def _reserve_literals(self, session_id: str, text: str) -> None:
        with self._lock:
            try:
                self._db.execute("BEGIN IMMEDIATE")
                self._reserve_literals_locked(session_id, text)
                self._db.commit()
            except Exception:
                self._db.rollback()
                raise

    def _reserve_literals_locked(self, session_id: str, text: str) -> None:
        numbers = [int(match.group(1)) for match in TOKEN_RE.finditer(text)]
        if not numbers:
            return
        seq = self._db.execute(
            "SELECT highest FROM token_seq WHERE session_id = ?",
            (session_id,),
        ).fetchone()
        highest = max(seq[0] if seq else 0, max(numbers))
        self._db.execute(
            """
            INSERT INTO token_seq (session_id, highest) VALUES (?, ?)
            ON CONFLICT(session_id) DO UPDATE SET
                highest = MAX(token_seq.highest, excluded.highest)
            """,
            (session_id, highest),
        )

    def _ranges(self, text: str, spans: list[dict]) -> list[tuple[int, int]]:
        ranges: list[tuple[int, int]] = []
        for span in spans:
            start = _offset(span, "start")
            end = _offset(span, "end")
            if start < 0 or end > len(text) or start >= end:
                raise MaskError("span is outside the text")
            ranges.append((start, end))
        last = 0
        for start, end in sorted(ranges):
            if start < last:
                raise MaskError("spans overlap")
            last = end
        return ranges

    def _token_for(self, session_id: str, value: str, source: str) -> str:
        row = self._db.execute(
            "SELECT token FROM masks WHERE session_id = ? AND value = ?",
            (session_id, value),
        ).fetchone()
        if row is not None:
            return row[0]
        seq = self._db.execute(
            "SELECT highest FROM token_seq WHERE session_id = ?",
            (session_id,),
        ).fetchone()
        highest = seq[0] if seq else 0
        # A literal ⟦pii:N⟧ already in the prompt is not one of our tokens.
        # Numbering past it keeps demask from filling that literal with a value.
        occupied = {int(match.group(1)) for match in TOKEN_RE.finditer(source)}
        number = highest + 1
        while number in occupied:
            number += 1
        token = f"⟦pii:{number}⟧"
        top = max([number, *occupied])
        self._db.execute(
            "INSERT INTO masks (session_id, token, value, created_at) VALUES (?, ?, ?, ?)",
            (session_id, token, value, int(time.time())),
        )
        self._db.execute(
            """
            INSERT INTO token_seq (session_id, highest) VALUES (?, ?)
            ON CONFLICT(session_id) DO UPDATE SET
                highest = MAX(token_seq.highest, excluded.highest)
            """,
            (session_id, top),
        )
        return token


def store_for(harness: str, root: Path | None = None) -> MaskStore:
    """Open <root>/<harness>.sqlite, creating the directory.

    root defaults to ~/.config/wally/pls/masks, honoring WALLY_PROFILE_DIR
    the same way pls.PROFILE_DIR does. Tests pass an explicit root.
    """
    if not isinstance(harness, str) or _HARNESS_RE.fullmatch(harness) is None:
        raise ValueError(f"bad harness id: {harness!r}")
    directory = Path(root) if root is not None else _profile_root()
    directory.mkdir(parents=True, exist_ok=True)
    os.chmod(directory, 0o700)
    path = directory / f"{harness}.sqlite"
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    os.close(fd)
    os.chmod(path, 0o600)
    return MaskStore(path)
