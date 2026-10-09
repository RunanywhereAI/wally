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
    WALLY_BROWSER_USER_DATA_DIR   dedicated: a profile directory to keep (default: a fresh one per run)
    WALLY_BROWSER_EXECUTABLE      dedicated: Chrome/Chromium binary (default: browser-use finds one)
    WALLY_BROWSER_MAX_STEPS       default 60
    WALLY_BROWSER_DEADLINE        epoch seconds the session key expires; the run ends a minute before
    WALLY_BROWSER_LOG_DIR         where the per-step log goes (default ~/.local/state/wally/browser)
"""

from __future__ import annotations

import argparse
import asyncio
import os
import re
import sys
import time
import traceback
from pathlib import Path

from . import dialogs  # noqa: F401  (package __init__ has already turned telemetry off)
from . import leave_the_dock
from .decisions import DecisionClient
from .policy import EvePolicy
from .profile import ProfileError, load
from .text import TextModel
from . import install_log_filter, quiet_library_logs
from .tools import GuardContext, build_tools, terminal_ask


HUMAN_STEP_TIMEOUT_S = 1800
# Closing the last window can leave Chrome running with no pages for a moment.
# Three polls is long enough to ignore that gap and short enough to feel immediate.
EMPTY_PAGE_POLLS = 3
BROWSER_POLL_S = 0.4


def browser_should_stop(
    process_running: bool | None, page_count: int | None, seen_page: bool, empty_polls: int
) -> tuple[bool, bool, int]:
    """Whether the browser the person was looking at is gone.

    `process_running` is None when this run did not launch Chrome (attach).
    `page_count` is None when the page list could not be read. A dead process
    stops at once. Pages have to be gone for a few polls, so a navigation that
    briefly reports no pages does not end the run.
    """
    if process_running is False:
        return True, seen_page, empty_polls
    if page_count is None:
        if not seen_page:
            return False, seen_page, empty_polls
        page_count = 0
    if page_count > 0:
        return False, True, 0
    if not seen_page:
        return False, False, 0
    empty_polls += 1
    return empty_polls >= EMPTY_PAGE_POLLS, seen_page, empty_polls


def chrome_process_running(session) -> bool | None:
    """True/False for a Chrome this run launched. None when it did not (attach)."""
    watchdog = getattr(session, "_local_browser_watchdog", None)
    process = getattr(watchdog, "_subprocess", None) if watchdog is not None else None
    if process is None:
        return None
    try:
        return bool(process.is_running())
    except Exception:
        return False


async def watch_until_browser_closes(session) -> None:
    """Ends this process once the person closes the browser.

    `os._exit` is deliberate. AppKit, imported for the screen size, keeps a
    non-daemon thread that would hold the interpreter open after the agent
    stopped, which is the Dock icon that survives Ctrl-C and a closed window.
    """
    seen_page = False
    empty_polls = 0
    while True:
        await asyncio.sleep(BROWSER_POLL_S)
        running = chrome_process_running(session)
        try:
            pages: int | None = len(session.get_page_targets())
        except Exception:
            pages = None
        stop, seen_page, empty_polls = browser_should_stop(running, pages, seen_page, empty_polls)
        if not stop:
            continue
        sys.stderr.write("\nbrowser closed\n")
        if running is not None:
            try:
                await asyncio.wait_for(session.kill(), timeout=2)
            except Exception:
                pass
        os._exit(0)


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


# Chrome's bad-flags infobar (chrome/browser/ui/startup/bad_flags_prompt.cc)
# names these two. The first only lets an extension run on chrome:// pages.
# The second is a developer Blink switch; with a fixed DevTools port Chrome
# does not turn AutomationControlled on, so disabling it only draws the bar.
CHROME_ARGS_TO_DROP = [
    "--extensions-on-chrome-urls",
    "--disable-blink-features=AutomationControlled",
]

PROFILE_PREFIX = "wally-browser-"
PROFILE_MAX_AGE_S = 24 * 3600


def fresh_profile_dir() -> Path:
    """A new, empty browser profile for this run, so no login, saved card or 1-click setting from
    an earlier run is there. The browser stays open on the last page when the run ends, so the
    directory is swept on a later run once it is a day old, not deleted on exit."""
    import shutil
    import tempfile

    base = Path(tempfile.gettempdir())
    now = time.time()
    for old in base.glob(f"{PROFILE_PREFIX}*"):
        try:
            if now - old.stat().st_mtime > PROFILE_MAX_AGE_S:
                shutil.rmtree(old, ignore_errors=True)
        except OSError:
            pass
    return Path(tempfile.mkdtemp(prefix=PROFILE_PREFIX))


_DEBUG_PORT = re.compile(r"--remote-debugging-port=(\d+)")


def cdp_from_command_lines(lines: list[str]) -> str | None:
    """The DevTools address of a Chrome this agent already launched.

    Only a process whose profile directory is `wally-browser-*` counts. The
    person's own Chrome, Brave, or Edge is left alone. Helper processes
    (`--type=`) are not the browser.
    """
    for line in lines:
        if "wally-browser-" not in line or "--type=" in line:
            continue
        match = _DEBUG_PORT.search(line)
        if match:
            return f"http://127.0.0.1:{match.group(1)}"
    return None


def running_wally_cdp() -> str | None:
    import subprocess
    import urllib.request

    try:
        listed = subprocess.check_output(["ps", "-axww", "-o", "command="], text=True)
    except (OSError, subprocess.CalledProcessError):
        return None
    url = cdp_from_command_lines(listed.splitlines())
    if not url:
        return None
    try:
        urllib.request.urlopen(url + "/json/version", timeout=0.4)
    except OSError:
        return None
    return url


def build_browser(chrome: str) -> tuple[object, bool]:
    """The session, and whether it is an existing Wally Chrome (a new tab, not a new app)."""
    from browser_use import BrowserProfile, BrowserSession

    if chrome == "attach":
        cdp_url = _env("WALLY_BROWSER_CDP_URL", "http://127.0.0.1:9222")
        profile = BrowserProfile(cdp_url=cdp_url, keep_alive=True)
        return BrowserSession(browser_profile=profile), True
    if chrome != "dedicated":
        raise ConfigError(f"WALLY_BROWSER_CHROME must be dedicated or attach, not {chrome!r}")
    existing = None if _env("WALLY_BROWSER_USER_DATA_DIR") else running_wally_cdp()
    if existing:
        profile = BrowserProfile(cdp_url=existing, keep_alive=True)
        return BrowserSession(browser_profile=profile), True
    kept = _env("WALLY_BROWSER_USER_DATA_DIR")
    user_data_dir = Path(kept) if kept else fresh_profile_dir()
    user_data_dir.mkdir(parents=True, exist_ok=True)
    profile = BrowserProfile(user_data_dir=str(user_data_dir), headless=False, keep_alive=True,
                             executable_path=_env("WALLY_BROWSER_EXECUTABLE"),
                             ignore_default_args=CHROME_ARGS_TO_DROP)
    return BrowserSession(browser_profile=profile), False


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

    if os.environ.get("WALLY_BROWSER_CLEAN") == "1":
        quiet_library_logs()
    else:
        install_log_filter()
    dialogs.install()
    loop = asyncio.get_running_loop()

    def _ignore_shutdown_noise(loop, context):  # noqa: ARG001
        text = f"{context.get('message', '')} {context.get('exception', '')}"
        if "aclose" in text or "asynchronous generator" in text or "CDP" in text:
            return
        loop.default_exception_handler(context)

    loop.set_exception_handler(_ignore_shutdown_noise)
    decisions = DecisionClient(base, key, decision_model, prompt_format_version=None)
    text = TextModel(base, key, text_model)
    if not start_url:
        try:
            start_url = text.opening_url(goal)
        except Exception as error:
            sys.stderr.write(f"could not choose a start address: {error}\n")
            start_url = None
        if not start_url:
            typed = terminal_ask("Can you paste the site address or the URL, so I can start?")
            if not typed:
                decisions.close()
                text.close()
                return 2
            start_url = typed.strip()
    stop_messages: list[str] = []
    chrome = _env("WALLY_BROWSER_CHROME", "dedicated")
    # A kept profile collects logins and saved payment methods like the person's own Chrome, so it
    # gets the same confirm-everything rule.
    guard = GuardContext(attach=chrome == "attach" or bool(_env("WALLY_BROWSER_USER_DATA_DIR")))
    tools = build_tools(ask=terminal_ask, on_stop=stop_messages.append, context=guard)
    session, reused = build_browser(chrome)
    leave_the_dock()
    task = goal if not start_url else f"{goal} (start at {start_url})"
    # browser-use's own model slot: GLM on the same API. eve chooses the actions;
    # this is set so any browser-use path that still calls a model stays on our API.
    llm = ChatOpenAI(model=text_model, base_url=base, api_key=key)

    async def should_stop() -> bool:
        return bool(deadline) and time.time() > deadline - 60

    agent = EveAgent(
        task=task, llm=llm, browser_session=session, tools=tools,
        policy=EvePolicy(decisions, profile), text_model=text, profile=profile, ask=terminal_ask,
        log_path=log_path, guard_context=guard, use_vision=True, use_judge=False, enable_planning=False, message_compaction=False,
        final_response_after_failure=False, max_actions_per_step=1, calculate_cost=False,
        include_tool_call_examples=False, register_should_stop_callback=should_stop,
        # A step can include the person (a CAPTCHA, a y/N, a missing detail); browser-use's 180 s
        # defaults would time them out and leave a stdin reader behind. The decision and text calls
        # carry their own 60 s HTTP timeouts.
        llm_timeout=HUMAN_STEP_TIMEOUT_S, step_timeout=HUMAN_STEP_TIMEOUT_S,
        # browser-use's own Ctrl-C pauses and, when the tab detaches, opens a new
        # about:blank. wally already ends the process; that recovery must not run.
        enable_signal_handler=False,
        initial_actions=[{"navigate": {"url": start_url, "new_tab": reused}}] if start_url else None,
    )
    agent._wally_chat = [f"Person: {goal}"]
    started = time.perf_counter()
    closed_task = asyncio.create_task(watch_until_browser_closes(session))
    history = None
    # browser-use closes its event queue at the end of every run(). A follow-up
    # task then dies with QueueShutDown and the shell sits there. Hold the real
    # close until the person presses Enter.
    real_stop = agent.eventbus.stop
    real_close = agent.close

    async def _keep_open(*_args, **_kwargs):
        return None

    agent.eventbus.stop = _keep_open
    agent.close = _keep_open
    try:
        while True:
            agent_task = asyncio.create_task(agent.run(max_steps=max_steps))
            done, _pending = await asyncio.wait(
                {agent_task, closed_task}, return_when=asyncio.FIRST_COMPLETED
            )
            # The watcher ends the process itself. Reaching here means the agent finished
            # and the browser is still the one the person left open.
            if agent_task not in done:
                return 0
            history = await agent_task
            reason = agent.stop_reason or (stop_messages[-1] if stop_messages else "")
            if "payment" in reason.lower():
                prompt = ("\nStopped at payment. Finish it in the browser, then press Enter "
                          "and I will continue from here.\n> ")
            else:
                prompt = f"\n{reason}\nWhat should I do next? Press Enter to stop.\n> "
            sys.stderr.write(prompt)
            sys.stderr.flush()
            answer = await asyncio.to_thread(sys.stdin.readline)
            if not answer.strip():
                break
            if "payment" not in reason.lower():
                page = ""
                try:
                    page = await agent.browser_session.get_current_page_url()
                except Exception:
                    page = ""
                agent._wally_chat.append(f"Stopped: {reason}. Page: {page}")
                agent._wally_chat.append(f"Person: {answer.strip()}")
                agent.task = (
                    "Conversation so far:\n" + "\n".join(agent._wally_chat) +
                    "\nDo the person's latest message. The open page is what they are reacting to. "
                    "If they reject it, leave it."
                )
                agent._wally_plan = ""
                # A second run() replays initial_actions, which are already ActionModel
                # objects, and browser-use rejects them. This is a follow-up on the open page.
                agent.initial_actions = None
                if getattr(agent, "state", None) is not None:
                    agent.state.follow_up_task = True
                try:
                    nxt = text.opening_url(agent.task)
                except Exception:
                    nxt = None
                if nxt and nxt != page:
                    await agent.browser_session.navigate_to(nxt, new_tab=True)
            agent.stop_reason = ""
            stop_messages.clear()
            if getattr(agent, "state", None) is not None:
                agent.state.stopped = False
    finally:
        closed_task.cancel()
        install_log_filter()
        agent.eventbus.stop = real_stop
        agent.close = real_close
        try:
            await real_stop(clear=True, timeout=1.0)
        except Exception:
            pass
        try:
            await real_close()
        except Exception:
            pass
        agent.finish()
        decisions.close()
        text.close()
    wall = time.perf_counter() - started

    steps = agent._wally_steps
    reason = agent.stop_reason or (stop_messages[-1] if stop_messages else
                                   ("ran out of time" if deadline and time.time() > deadline - 60 else
                                    f"stopped after {len(steps)} steps"))
    if os.environ.get("WALLY_BROWSER_CLEAN") == "1":
        sys.stderr.write("\n")
        sys.stderr.write(f"result  {reason}\n")
        sys.stderr.write(f"steps   {len(steps)}   {wall:.0f}s\n")
        sys.stderr.write(f"log     {log_path}\n")
        sys.stderr.write("The browser stays open.\n")
    else:
        decided = [s for s in steps if s.decision_ms]
        sys.stderr.write("\n— wally browser-use —\n")
        sys.stderr.write(f"result: {reason}\n")
        if decided:
            avg_decision = sum(s.decision_ms for s in decided) / len(decided)
            timed = [s.step_ms for s in steps if s.step_ms]
            avg_step = sum(timed) / len(timed) if timed else 0
            sys.stderr.write(f"steps: {len(steps)}  decision avg {avg_decision:.0f} ms  "
                             f"step avg {avg_step:.0f} ms  wall {wall:.1f} s  plan {agent.plan_ms} ms\n")
        sys.stderr.write(f"log: {log_path}\n")
        sys.stderr.write("The browser stays open on the last page.\n")
    return 0 if history and history.is_successful() else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="wally browser-use", description=__doc__.split("\n")[0])
    parser.add_argument("goal", help="what to do, in plain words")
    parser.add_argument("--start-url", help="open this page first")
    args = parser.parse_args(argv)
    code = 1
    try:
        code = asyncio.run(run(args.goal, args.start_url))
    except (ConfigError, ProfileError) as error:
        sys.stderr.write(f"Error: {error}\n")
        code = 2
    except KeyboardInterrupt:
        sys.stderr.write("\nstopped by the person\n")
        code = 130
    except Exception:
        sys.stderr.write(traceback.format_exc())
        code = 1
    # Same reason as watch_until_browser_closes: a normal return would leave
    # the AppKit thread, and the Dock icon, behind.
    os._exit(code)
