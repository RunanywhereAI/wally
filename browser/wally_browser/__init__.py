"""wally browser-use: browser-use driven by a decision model, hard stops in code.

browser-use reports usage to PostHog and syncs to its cloud unless told not to,
and it reads both switches when it is first imported. Set them here, before
anything imports browser_use, and set them unconditionally.
"""

import os
import sys

os.environ["ANONYMIZED_TELEMETRY"] = "false"
os.environ["BROWSER_USE_CLOUD_SYNC"] = "false"


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
