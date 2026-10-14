"""Pick the desktop backend for the running platform.

Set DESKTOP_MCP_BACKEND to "kde", "hyprland" or "windows" to override detection.
"""

from __future__ import annotations

import os
import sys

from .base import Backend


def detect() -> str:
    forced = os.environ.get("DESKTOP_MCP_BACKEND", "").strip().lower()
    if forced:
        return forced
    if sys.platform == "win32":
        return "windows"
    if sys.platform.startswith("linux"):
        if os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
            return "hyprland"
        return "kde"
    raise RuntimeError(f"unsupported platform: {sys.platform}")


def load_backend() -> Backend:
    name = detect()
    if name == "windows":
        from .windows import WindowsBackend
        return WindowsBackend()
    if name == "hyprland":
        from .linux_hyprland import HyprlandBackend
        return HyprlandBackend()
    if name == "kde":
        from .linux_kde import KdeBackend
        return KdeBackend()
    raise RuntimeError(f"unknown DESKTOP_MCP_BACKEND '{name}' (kde, hyprland, windows)")
