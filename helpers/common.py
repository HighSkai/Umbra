"""Shared plumbing for Umbra's helpers.

Protocol: the core writes one JSON object (token, hardware id, arguments) as the
first line on stdin, then may send more JSON lines (a 2FA code, WebRTC messages,
"close"). The helper answers with JSON lines on stdout:

  {"type": "token", "token": {...}}        refreshed credentials, saved by the core
  {"type": "result", ...}                  poll result
  {"type": "need_2fa", "message": "..."}   waiting for a code on stdin
  {"type": "done"}                         login finished
  {"type": "ready", ...}                   live view is ready
  {"type": "error", "code": "...", "message": "..."}

Secrets arrive only on stdin, never in argv or the environment. Library logging
is silenced because it can print tokens; stderr carries only our own short notes.
"""

from __future__ import annotations

import asyncio
import datetime as dt
import json
import logging
import sys
import threading
from typing import Any

logging.basicConfig(stream=sys.stderr, level=logging.WARNING, format="%(levelname)s %(message)s")
for noisy in ("blinkpy", "ring_doorbell", "aiohttp", "websockets", "firebase_messaging", "asyncio"):
    logging.getLogger(noisy).setLevel(logging.CRITICAL)
log = logging.getLogger("umbra")


def emit(obj: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":"), default=str) + "\n")
    sys.stdout.flush()


def error(code: str, message: str) -> None:
    emit({"type": "error", "code": code, "message": message})


def read_first() -> dict[str, Any]:
    line = sys.stdin.readline()
    try:
        return json.loads(line) if line.strip() else {}
    except ValueError:
        return {}


class Inbox:
    """JSON lines that arrive on stdin after the first one, delivered to asyncio.

    A thread does the blocking reads so this works the same on Linux and Windows.
    EOF (the core went away or closed the pipe) arrives as {"type": "eof"}.
    """

    def __init__(self) -> None:
        self.loop = asyncio.get_running_loop()
        self.queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
        threading.Thread(target=self._run, daemon=True).start()

    def _run(self) -> None:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            self.loop.call_soon_threadsafe(self.queue.put_nowait, msg)
        self.loop.call_soon_threadsafe(self.queue.put_nowait, {"type": "eof"})

    async def get(self, timeout: float | None = None) -> dict[str, Any]:
        return await asyncio.wait_for(self.queue.get(), timeout)


def to_ts(value: Any) -> int | None:
    """Epoch seconds from a datetime, an ISO-8601 string or a number."""
    if value is None:
        return None
    if isinstance(value, dt.datetime):
        if value.tzinfo is None:
            value = value.replace(tzinfo=dt.timezone.utc)
        return int(value.timestamp())
    if isinstance(value, (int, float)):
        return int(value / 1000 if value > 1e12 else value)
    if isinstance(value, str):
        s = value.strip().replace("Z", "+00:00")
        try:
            return to_ts(dt.datetime.fromisoformat(s))
        except ValueError:
            return None
    return None


def short(exc: BaseException) -> str:
    text = str(exc).strip() or exc.__class__.__name__
    return text.splitlines()[0][:200]


def run(main) -> None:
    try:
        code = asyncio.run(main())
    except KeyboardInterrupt:
        code = 1
    except Exception as exc:  # never leave the core without an answer
        error("crashed", f"Helper crashed: {short(exc)}")
        code = 1
    sys.exit(code or 0)
