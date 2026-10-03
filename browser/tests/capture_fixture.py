"""Capture a real page as a guard fixture: the same pipeline the agent uses
(browser-use's page state → wally_browser.elements.element_from_node), saved as
element descriptors and frame URLs only. No page body, no screenshots.

    uv run python -m tests.capture_fixture NAME URL [--wait SECONDS]

Writes tests/fixtures/real/NAME.json. Only capture public pages with no
personal data on them (payment-gateway demo pages, search results); check the
file before committing it.
"""

from __future__ import annotations

import argparse
import asyncio
import json
from dataclasses import asdict
from pathlib import Path

import wally_browser  # noqa: F401  (telemetry off before browser_use is imported)
from wally_browser.agent import page_frames
from wally_browser.elements import elements_from_selector_map

from .test_e2e import CHROME

KEEP_ATTRIBUTES = {"id", "name", "type", "role", "aria-label", "title", "alt", "href", "placeholder",
                   "autocomplete", "for", "class", "data-testid", "contenteditable", "formaction", "value"}
OUT = Path(__file__).parent / "fixtures" / "real"


async def capture(name: str, url: str, wait_s: float) -> Path:
    from browser_use import BrowserProfile, BrowserSession

    session = BrowserSession(browser_profile=BrowserProfile(executable_path=CHROME, headless=True,
                                                            keep_alive=False))
    await session.start()
    try:
        await session.navigate_to(url)
        await asyncio.sleep(wait_s)
        state = await session.get_browser_state_summary(include_screenshot=False)
        elements = elements_from_selector_map(state.dom_state.selector_map)
        try:
            all_frames, _ = await session.get_all_frames()
            tracked = [info.get("url", "") for info in all_frames.values()]
        except Exception:
            tracked = []
        record = {
            "url": state.url,
            "title": state.title,
            "frames": sorted({src for src, _ in page_frames(state.dom_state)} | set(tracked)),
            "visible_frames": sorted({src for src, visible in page_frames(state.dom_state) if visible}),
            "elements": [{**{k: v for k, v in asdict(e).items() if k != "attributes"},
                          "attributes": {k: v for k, v in e.attributes.items() if k in KEEP_ATTRIBUTES}}
                         for e in elements],
        }
    finally:
        await session.kill()
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / f"{name}.json"
    path.write_text(json.dumps(record, indent=1, ensure_ascii=False))
    return path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("name")
    parser.add_argument("url")
    parser.add_argument("--wait", type=float, default=4.0)
    args = parser.parse_args()
    print(asyncio.run(capture(args.name, args.url, args.wait)))


if __name__ == "__main__":
    main()
