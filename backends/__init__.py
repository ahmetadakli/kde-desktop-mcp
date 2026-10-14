"""Pick the desktop backend for the running platform."""

from __future__ import annotations

import sys

from .base import Backend


def load_backend() -> Backend:
    if sys.platform == "win32":
        from .windows import WindowsBackend
        return WindowsBackend()
    if sys.platform.startswith("linux"):
        from .linux_kde import KdeBackend
        return KdeBackend()
    raise RuntimeError(f"unsupported platform: {sys.platform}")
