"""The text model (GLM-5.3-Flash on the same API): the trip plan at the start,
and the words for a field the traveller profile does not cover ("Bangalore" in
a destination box). It never chooses an action, and it is never asked for a
value of a field guards.py refuses.
"""

from __future__ import annotations

import httpx

PLAN_PROMPT = (
    "You plan a web task for a browser agent. Reply with 3 to 8 short numbered steps, one line each, "
    "and nothing else. The agent must stop at the payment page and never pay. "
    "The task may include the conversation so far. Do what the person just asked, "
    "using the earlier turns. If they reject the page already open, leave it. Task:\n{goal}"
)
URL_PROMPT = (
    "A browser agent was given this task and no web address.\n"
    "Task: {goal}\n"
    "Reply with exactly one https URL where the task should start, and nothing else. "
    "If you cannot know the site without the person naming it, reply with exactly: ASK"
)

FIELD_PROMPT = (
    "A browser agent is doing this task: {goal}\n"
    "Plan:\n{plan}\n"
    "Page: {page}\n"
    "Field to fill: {field}\n"
    "Reply with only the exact text to type into that field, nothing else. If the task does not say what "
    "belongs there, reply with exactly: ASK"
)


class TextError(RuntimeError):
    pass


def parse_opening_url(text: str) -> str | None:
    text = text.strip().strip('"').strip()
    if not text or text.upper() == "ASK":
        return None
    url = text.split()[0]
    if url.startswith("https://") or url.startswith("http://"):
        return url
    return None


class TextModel:
    def __init__(self, base_url: str, api_key: str, model: str, *, timeout_s: float = 60.0,
                 transport: httpx.BaseTransport | None = None):
        self.url = base_url.rstrip("/") + "/chat/completions"
        self.model = model
        self._headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json",
                         "User-Agent": "wally-browser/0.1"}
        self._client = httpx.Client(timeout=timeout_s, transport=transport)

    def close(self) -> None:
        self._client.close()

    def _complete(self, prompt: str, max_tokens: int) -> str:
        response = self._client.post(self.url, headers=self._headers, json={
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens,
            "temperature": 0.2,
            "reasoning_effort": "none",
        })
        if response.status_code != 200:
            try:
                message = response.json().get("error", {}).get("message", "")
            except ValueError:
                message = ""
            raise TextError(f"text model refused (HTTP {response.status_code}) {message[:200]}".strip())
        choices = response.json().get("choices") or []
        content = (choices[0].get("message") or {}).get("content") if choices else None
        if not content:
            raise TextError("text model returned no text")
        return content.strip()

    def plan(self, goal: str) -> str:
        return self._complete(PLAN_PROMPT.format(goal=goal), 300)

    def opening_url(self, goal: str) -> str | None:
        """A start URL, or None when the person has to name the site."""
        return parse_opening_url(self._complete(URL_PROMPT.format(goal=goal), 80))

    def field_text(self, goal: str, plan: str, page: str, field: str) -> str | None:
        """The text to type, or None when the task does not say (ask the user)."""
        text = self._complete(FIELD_PROMPT.format(goal=goal, plan=plan or "-", page=page, field=field), 60)
        text = text.strip().strip('"').strip()
        if not text or text.upper() == "ASK" or "\n" in text:
            return None
        return text
