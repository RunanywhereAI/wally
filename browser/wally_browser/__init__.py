"""wally browser-use: browser-use driven by a decision model, hard stops in code.

browser-use reports usage to PostHog and syncs to its cloud unless told not to,
and it reads both switches when it is first imported. Set them here, before
anything imports browser_use, and set them unconditionally.
"""

import logging
import os
import sys

os.environ["ANONYMIZED_TELEMETRY"] = "false"
os.environ["BROWSER_USE_CLOUD_SYNC"] = "false"


class _HideShutdown(logging.Filter):
    """The CDP socket always complains while Chrome is closed. That is not a
    failure of the task, in either output style."""

    _DROP = (
        "CDP WebSocket",
        "WebSocket reconnection",
        "Cleared all owned data",
        "asynchronous generator",
        "aclose()",
    )

    def filter(self, record: logging.LogRecord) -> bool:
        message = record.getMessage()
        return not any(part in message for part in self._DROP)


def install_log_filter() -> None:
    """Attach to every logger and handler. A filter on a parent does not see
    records a child logger emits through its own handler."""
    filt = _HideShutdown()
    loggers = [logging.getLogger()]
    for name, obj in logging.root.manager.loggerDict.items():
        if isinstance(obj, logging.Logger) and name.startswith(("browser_use", "cdp_use", "asyncio", "bubus")):
            loggers.append(obj)
    for name in ("browser_use", "cdp_use", "asyncio", "bubus"):
        loggers.append(logging.getLogger(name))
    for logger in loggers:
        logger.addFilter(filt)
        for handler in logger.handlers:
            handler.addFilter(filt)


def quiet_library_logs() -> None:
    """browser-use logs every step and a version nag. Used only for --clean-output."""
    logging.basicConfig(level=logging.ERROR, force=True)
    for name in ("browser_use", "cdp_use", "httpx", "httpcore", "asyncio"):
        logging.getLogger(name).setLevel(logging.CRITICAL)
    install_log_filter()


install_log_filter()


def leave_the_dock() -> None:
    """browser-use imports AppKit for the screen size, and that makes this
    interpreter a foreground Mac app: a Dock icon, and a process that outlives
    the terminal. Accessory would still show a menu bar. Prohibited does not.
    Set before browser-use imports AppKit, and again is harmless."""
    if sys.platform != "darwin":
        return
    try:
        from AppKit import NSApplication, NSApplicationActivationPolicyProhibited
    except ImportError:
        return
    NSApplication.sharedApplication().setActivationPolicy_(
        NSApplicationActivationPolicyProhibited
    )


leave_the_dock()
