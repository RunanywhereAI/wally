"""JavaScript dialogs, answered by guards.dialog_should_accept instead of
browser-use's PopupsWatchdog, which accepts confirm() and beforeunload. A
"Confirm payment?" or "Leave this page?" dialog is dismissed, never accepted.

browser-use imports PopupsWatchdog from its module when a session attaches its
watchdogs, so install() swaps the module attribute before the session starts.
"""

from __future__ import annotations

import asyncio

from browser_use.browser.events import TabCreatedEvent
from browser_use.browser.watchdogs import popups_watchdog

from .guards import dialog_should_accept


class WallyDialogWatchdog(popups_watchdog.PopupsWatchdog):
    """Same registration as PopupsWatchdog; a different answer."""

    async def on_TabCreatedEvent(self, event: TabCreatedEvent) -> None:
        target_id = event.target_id
        if target_id in self._dialog_listeners_registered:
            return
        session = self.browser_session
        try:
            cdp_session = await session.get_or_create_cdp_session(target_id, focus=False)
            try:
                await cdp_session.cdp_client.send.Page.enable(session_id=cdp_session.session_id)
            except Exception:
                pass

            async def handle_dialog(event_data, session_id: str | None = None):
                dialog_type = event_data.get("type", "alert")
                message = event_data.get("message", "")
                accept = dialog_should_accept(dialog_type)
                verb = "accepted" if accept else "dismissed"
                session._closed_popup_messages.append(f"[{dialog_type}, {verb} by wally] {message}"[:300])
                self.logger.info(f"wally: JavaScript {dialog_type} dialog {verb}: {message[:100]!r}")
                for sid in (session_id, cdp_session.session_id):
                    if not sid or session._cdp_client_root is None:
                        continue
                    try:
                        await asyncio.wait_for(
                            session._cdp_client_root.send.Page.handleJavaScriptDialog(
                                params={"accept": accept}, session_id=sid),
                            timeout=0.5,
                        )
                        return
                    except Exception:
                        continue

            cdp_session.cdp_client.register.Page.javascriptDialogOpening(handle_dialog)  # type: ignore[arg-type]
            if hasattr(session._cdp_client_root, "register"):
                try:
                    session._cdp_client_root.register.Page.javascriptDialogOpening(handle_dialog)  # type: ignore[arg-type]
                except Exception:
                    pass
            self._dialog_listeners_registered.add(target_id)
        except Exception as error:
            self.logger.warning(f"wally: could not set up dialog handling for tab {target_id}: {error}")


def install() -> None:
    """Make every BrowserSession started after this use WallyDialogWatchdog."""
    popups_watchdog.PopupsWatchdog = WallyDialogWatchdog
