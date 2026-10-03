"""`python -m wally_browser "<goal>"`: what `wally browser-use` launches.

Settings come from the environment Wally sets (nothing secret is read from a
file or printed):

    WALLY_BROWSER_API_BASE        https://inference.runanywhere.ai/v1
    WALLY_BROWSER_API_KEY         the hosted session key (bearer)
    WALLY_BROWSER_DECISION_MODEL  eve
    WALLY_BROWSER_TEXT_MODEL      glm-5.3-flash
    WALLY_BROWSER_PROFILE         traveller profile file (default ~/.config/wally/traveller.toml)
    WALLY_BROWSER_CHROME          dedicated (default) | attach
    WALLY_BROWSER_CDP_URL         attach: the running Chrome's DevTools URL
    WALLY_BROWSER_USER_DATA_DIR   dedicated: profile directory (default ~/.local/share/wally/browser-profile)
    WALLY_BROWSER_EXECUTABLE      dedicated: Chrome/Chromium binary (default: browser-use finds one)
    WALLY_BROWSER_MAX_STEPS       default 60
    WALLY_BROWSER_DEADLINE        epoch seconds the session key expires; the run ends a minute before
    WALLY_BROWSER_LOG_DIR         where the per-step log goes (default ~/.local/state/wally/browser)
"""

from __future__ import annotations

import argparse
import asyncio
import os
import sys
import time
from pathlib import Path

from . import dialogs  # noqa: F401  (package __init__ has already turned telemetry off)
from .decisions import DecisionClient
from .policy import EvePolicy
from .profile import ProfileError, load
from .text import TextModel
from .tools import GuardContext, build_tools, terminal_ask


HUMAN_STEP_TIMEOUT_S = 1800


class ConfigError(ValueError):
    pass


def _env(name: str, default: str | None = None, required: bool = False) -> str | None:
    value = os.environ.get(name) or default
    if required and not value:
        raise ConfigError(f"{name} is not set (run this through `wally browser-use`)")
    return value


def _state_dir() -> Path:
    base = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
    return Path(base) / "wally" / "browser"


def _data_dir() -> Path:
    base = os.environ.get("XDG_DATA_HOME") or str(Path.home() / ".local" / "share")
    return Path(base) / "wally" / "browser-profile"


def build_browser(chrome: str):
    from browser_use import BrowserProfile, BrowserSession

    if chrome == "attach":
        cdp_url = _env("WALLY_BROWSER_CDP_URL", "http://127.0.0.1:9222")
        profile = BrowserProfile(cdp_url=cdp_url, keep_alive=True)
    elif chrome == "dedicated":
        user_data_dir = Path(_env("WALLY_BROWSER_USER_DATA_DIR", str(_data_dir())))
        user_data_dir.mkdir(parents=True, exist_ok=True)
        profile = BrowserProfile(user_data_dir=str(user_data_dir), headless=False, keep_alive=True,
                                 executable_path=_env("WALLY_BROWSER_EXECUTABLE"))
    else:
        raise ConfigError(f"WALLY_BROWSER_CHROME must be dedicated or attach, not {chrome!r}")
    return BrowserSession(browser_profile=profile)


async def run(goal: str, start_url: str | None) -> int:
    from browser_use.llm.openai.chat import ChatOpenAI

    from .agent import EveAgent

    base = _env("WALLY_BROWSER_API_BASE", required=True)
    key = _env("WALLY_BROWSER_API_KEY", required=True)
    decision_model = _env("WALLY_BROWSER_DECISION_MODEL", "eve")
    text_model = _env("WALLY_BROWSER_TEXT_MODEL", "glm-5.3-flash")
    max_steps = int(_env("WALLY_BROWSER_MAX_STEPS", "60"))
    deadline = float(_env("WALLY_BROWSER_DEADLINE", "0") or 0)
    profile = load(_env("WALLY_BROWSER_PROFILE"))

    log_dir = Path(_env("WALLY_BROWSER_LOG_DIR", str(_state_dir())))
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / f"run-{time.strftime('%Y%m%d-%H%M%S')}.jsonl"

    dialogs.install()
    decisions = DecisionClient(base, key, decision_model, prompt_format_version=None)
    text = TextModel(base, key, text_model)
    stop_messages: list[str] = []
    chrome = _env("WALLY_BROWSER_CHROME", "dedicated")
    guard = GuardContext(attach=chrome == "attach")
    tools = build_tools(ask=terminal_ask, on_stop=stop_messages.append, context=guard)
    session = build_browser(chrome)
    task = goal if not start_url else f"{goal} (start at {start_url})"
    # browser-use's own model slot: GLM on the same API. eve chooses the actions;
    # this is set so any browser-use path that still calls a model stays on our API.
    llm = ChatOpenAI(model=text_model, base_url=base, api_key=key)

    async def should_stop() -> bool:
        return bool(deadline) and time.time() > deadline - 60

    agent = EveAgent(
        task=task, llm=llm, browser_session=session, tools=tools,
        policy=EvePolicy(decisions, profile), text_model=text, profile=profile, ask=terminal_ask,
        log_path=log_path, guard_context=guard, use_vision=False, use_judge=False, enable_planning=False, message_compaction=False,
        final_response_after_failure=False, max_actions_per_step=1, calculate_cost=False,
        include_tool_call_examples=False, register_should_stop_callback=should_stop,
        # A step can include the person (a CAPTCHA, a y/N, a missing detail); browser-use's 180 s
        # defaults would time them out and leave a stdin reader behind. The decision and text calls
        # carry their own 60 s HTTP timeouts.
        llm_timeout=HUMAN_STEP_TIMEOUT_S, step_timeout=HUMAN_STEP_TIMEOUT_S,
        initial_actions=[{"navigate": {"url": start_url}}] if start_url else None,
    )
    started = time.perf_counter()
    try:
        history = await agent.run(max_steps=max_steps)
    finally:
        agent.finish()
        decisions.close()
        text.close()
    wall = time.perf_counter() - started

    steps = agent._wally_steps
    decided = [s for s in steps if s.decision_ms]
    sys.stderr.write("\n— wally browser-use —\n")
    reason = agent.stop_reason or (stop_messages[-1] if stop_messages else
                                   ("ran out of time" if deadline and time.time() > deadline - 60 else
                                    f"stopped after {len(steps)} steps"))
    sys.stderr.write(f"result: {reason}\n")
    if decided:
        avg_decision = sum(s.decision_ms for s in decided) / len(decided)
        timed = [s.step_ms for s in steps if s.step_ms]
        avg_step = sum(timed) / len(timed) if timed else 0
        sys.stderr.write(f"steps: {len(steps)}  decision avg {avg_decision:.0f} ms  step avg {avg_step:.0f} ms  "
                         f"wall {wall:.1f} s  plan {agent.plan_ms} ms\n")
    sys.stderr.write(f"log: {log_path}\n")
    sys.stderr.write("The browser stays open on the last page.\n")
    final = history.final_result() if history else None
    if final:
        print(final)
    return 0 if history and history.is_successful() else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="wally browser-use", description=__doc__.split("\n")[0])
    parser.add_argument("goal", help="what to do, in plain words")
    parser.add_argument("--start-url", help="open this page first")
    args = parser.parse_args(argv)
    try:
        return asyncio.run(run(args.goal, args.start_url))
    except (ConfigError, ProfileError) as error:
        sys.stderr.write(f"Error: {error}\n")
        return 2
    except KeyboardInterrupt:
        sys.stderr.write("\nstopped by the person\n")
        return 130
