"""Shared window and prefix-cache settings for the local model."""

import os

MAX_KV = int(os.environ.get("PLS_MAX_KV", "65536"))
MAX_OUT = int(os.environ.get("PLS_MAX_OUT", "8192"))
APC_BLOCKS = int(os.environ.get("PLS_APC_BLOCKS", "8192"))
APC_CHECKPOINTS = int(os.environ.get("PLS_APC_CHECKPOINTS", "32"))
APC_CHECKPOINT_INTERVAL = int(os.environ.get("PLS_APC_CHECKPOINT_INTERVAL", "4096"))
