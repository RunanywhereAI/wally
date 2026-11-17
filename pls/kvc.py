"""Session prefix for the local KV cache.

The cache reuses an identical token prefix. The system prompt and the
harness's own messages are the same for every sub-agent in one launch. The
task is not. Placing the task after the shared messages makes that prefix
the part every new sub-agent hits.
"""

from typing import Callable


def order(messages: list, stable_user: Callable[[dict], bool]) -> list:
    """Shared messages first, the task after them, then the dialogue."""
    end = 0
    while end < len(messages) and messages[end].get("role") not in ("assistant", "tool"):
        end += 1
    head, tail = messages[:end], messages[end:]
    stable: list = []
    task: list = []
    for message in head:
        if message.get("role") == "user" and not stable_user(message):
            task.append(message)
        else:
            stable.append(message)
    if not task:
        return messages
    first_task = next(
        index for index, message in enumerate(head)
        if message.get("role") == "user" and not stable_user(message)
    )
    shared_after = any(
        message.get("role") != "user" or stable_user(message)
        for message in head[first_task + 1:]
    )
    if not shared_after:
        return messages
    return stable + task + tail
