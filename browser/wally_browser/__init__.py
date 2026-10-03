"""wally browser-use: browser-use driven by a decision model, hard stops in code.

browser-use reports usage to PostHog and syncs to its cloud unless told not to,
and it reads both switches when it is first imported. Set them here, before
anything imports browser_use, and set them unconditionally.
"""

import os

os.environ["ANONYMIZED_TELEMETRY"] = "false"
os.environ["BROWSER_USE_CLOUD_SYNC"] = "false"
