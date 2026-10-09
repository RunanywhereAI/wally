"""Client for the decision API (`POST {base}/decisions`, the shape Wally's
pinned contract `wally-decisions-public-v1` describes).

One request carries up to 128 questions over one shared input. Answers come
back as probabilities over each question's labels. The key is sent as a bearer
token and never logged.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass, field

import httpx

MAX_QUESTIONS = 128
RETRY_STATUSES = (429, 503)
MAX_RETRIES = 2
MAX_RETRY_WAIT_S = 10


class DecisionError(RuntimeError):
    def __init__(self, message: str, status: int = 0, code: str = ""):
        super().__init__(message)
        self.status = status
        self.code = code


class WindowExceeded(DecisionError):
    """A question was longer than the model's per-question window: rebuild smaller."""


@dataclass
class Question:
    id: str
    kind: str  # "choice" | "yes_no" | "score"
    text: str
    options: list[str] = field(default_factory=list)  # choice: option names; score: level texts

    def to_json(self) -> dict:
        body: dict = {"id": self.id, "type": self.kind, "question": self.text}
        if self.kind == "choice":
            body["options"] = [{"name": name} for name in self.options]
        elif self.kind == "score":
            body["levels"] = list(self.options)
        return body


@dataclass
class Answer:
    probabilities: dict[str, float]
    label_mass: float = 1.0

    def top(self) -> tuple[str, float]:
        """The most probable label; on a tie the first label the server listed."""
        best = ("", -1.0)
        for name, p in self.probabilities.items():
            if p > best[1]:
                best = (name, p)
        return best

    def p(self, label: str) -> float:
        return self.probabilities.get(label, 0.0)


@dataclass
class Result:
    answers: dict[str, Answer]
    prompt_tokens: int
    latency_ms: int
    attempts: int


class DecisionClient:
    def __init__(self, base_url: str, api_key: str, model: str, *, timeout_s: float = 60.0,
                 prompt_format_version: int | None = None, transport: httpx.BaseTransport | None = None,
                 sleep=time.sleep):
        self.url = base_url.rstrip("/") + "/decisions"
        self.model = model
        self.prompt_format_version = prompt_format_version
        self._headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json",
                         "User-Agent": "wally-browser/0.1"}
        self._client = httpx.Client(timeout=timeout_s, transport=transport)
        self._sleep = sleep

    def close(self) -> None:
        self._client.close()

    def body(self, input_text: str, questions: list[Question], temperature: float | None = None,
             images: list[str] | None = None) -> dict:
        if not 1 <= len(questions) <= MAX_QUESTIONS:
            raise ValueError(f"1 to {MAX_QUESTIONS} questions per request, got {len(questions)}")
        body: dict = {"model": self.model, "input": input_text, "questions": [q.to_json() for q in questions]}
        if images:
            body["images"] = images
        if temperature is not None:
            body["temperature"] = temperature
        if self.prompt_format_version is not None:
            body["prompt_format_version"] = self.prompt_format_version
        return body

    def ask(self, input_text: str, questions: list[Question], temperature: float | None = None,
            images: list[str] | None = None) -> Result:
        body = self.body(input_text, questions, temperature, images)
        payload = json.dumps(body).encode()
        attempts = 0
        started = time.perf_counter()
        while True:
            attempts += 1
            response = self._client.post(self.url, content=payload, headers=self._headers)
            if response.status_code in RETRY_STATUSES and attempts <= MAX_RETRIES:
                wait = _retry_after(response)
                self._sleep(min(MAX_RETRY_WAIT_S, wait if wait is not None else 1))
                continue
            break
        latency_ms = round((time.perf_counter() - started) * 1000)
        if response.status_code != 200:
            raise _error(response)
        document = response.json()
        answers = {qid: Answer(probabilities=dict(answer.get("probabilities") or {}),
                               label_mass=float(answer.get("label_mass", 1.0)))
                   for qid, answer in (document.get("answers") or {}).items()}
        missing = [q.id for q in questions if q.id not in answers]
        if missing:
            raise DecisionError(f"the response left out questions: {', '.join(missing)}")
        usage = document.get("usage") or {}
        return Result(answers=answers, prompt_tokens=int(usage.get("prompt_tokens", 0)),
                      latency_ms=latency_ms, attempts=attempts)


def _retry_after(response: httpx.Response) -> int | None:
    raw = response.headers.get("Retry-After", "")
    try:
        return max(0, int(raw))
    except ValueError:
        return None


def _error(response: httpx.Response) -> DecisionError:
    status = response.status_code
    message, code = f"HTTP {status}", ""
    try:
        detail = (response.json() or {}).get("error") or {}
        message = str(detail.get("message") or message)[:500]
        code = str(detail.get("code") or "")
    except (ValueError, AttributeError):
        pass
    if status == 400 and ("token" in message.lower() and ("longer" in message.lower() or "window" in message.lower()
                                                          or "accept" in message.lower())):
        return WindowExceeded(message, status, code)
    return DecisionError(f"decision request refused (HTTP {status}): {message}", status, code)
