"""Hyprland backend: hyprctl for windows, cursor and geometry; AT-SPI for content.

Hyprland reports each window's pid, so AT-SPI applications are matched to
compositor windows by process id instead of trusting toolkit focus states.
Coordinates are Hyprland's global layout (logical) pixels throughout.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import time

from evdev import UInput, ecodes as e

from .base import Node
from .linux_common import LinuxAtspiBackend, frame_of, window_extents  # sets gi versions

from gi.repository import Atspi  # noqa: E402


def _hyprctl_json(*args: str):
    out = subprocess.run(["hyprctl", "-j", *args], capture_output=True, text=True,
                         timeout=5)
    if out.returncode != 0:
        raise RuntimeError(f"hyprctl {' '.join(args)} failed: {out.stderr.strip()}")
    return json.loads(out.stdout or "null")


def _hyprctl_dispatch(*args: str) -> None:
    out = subprocess.run(["hyprctl", "dispatch", *args], capture_output=True, text=True,
                         timeout=5)
    if out.returncode != 0 or out.stdout.strip() not in ("ok", ""):
        raise RuntimeError(f"hyprctl dispatch {' '.join(args)}: "
                           f"{(out.stdout + out.stderr).strip()}")


def screen_extents(acc: Atspi.Accessible):
    ext = Atspi.Component.get_extents(acc, Atspi.CoordType.SCREEN)
    if ext.width <= 0 or ext.height <= 0:
        raise RuntimeError("element has no on-screen area")
    return ext


class HyprlandBackend(LinuxAtspiBackend):
    platform = "linux-hyprland"
    apps_note = LinuxAtspiBackend.apps_note + (
        " On Hyprland, Qt apps may need QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 and the "
        "at-spi bus must be running in the session.")

    def __init__(self) -> None:
        super().__init__()
        self._btn: UInput | None = None

    # ------------------------------------------------------------ compositor state

    def _active_window(self) -> dict:
        aw = _hyprctl_json("activewindow")
        return aw if isinstance(aw, dict) and aw.get("address") else {}

    def _app_name_for_pid(self, pid: int) -> str:
        for a in self._applications():
            try:
                if a.get_process_id() == pid:
                    return a.get_name() or ""
            except Exception:
                continue
        return ""

    def _client_app_name(self, client: dict) -> str:
        # Prefer the AT-SPI name so one allowlist works for every Linux backend;
        # fall back to the window class for apps without accessibility.
        return (self._app_name_for_pid(client.get("pid", -1))
                or (client.get("class") or "")).lower()

    @staticmethod
    def _same_window(frame: Atspi.Accessible, app: Atspi.Accessible, client: dict) -> bool:
        if app.get_process_id() != client.get("pid"):
            return False
        if app.get_child_count() <= 1:
            return True
        return (frame.get_name() or "").strip() == (client.get("title") or "").strip()

    def _window_active(self, app: Atspi.Accessible, window: Atspi.Accessible) -> bool:
        aw = self._active_window()
        return bool(aw) and self._same_window(window, app, aw)

    def active_app(self) -> str:
        aw = self._active_window()
        return self._client_app_name(aw) if aw else ""

    # ------------------------------------------------------------ geometry and pointer

    def element_center(self, node: Node) -> tuple[int, int]:
        acc = node.acc
        aw = self._active_window()
        frame = frame_of(acc)
        if not aw or frame is None or not self._same_window(frame, acc.get_application(), aw):
            raise RuntimeError("element's window is not active; call activate_window first")
        if aw.get("xwayland"):
            # X11 clients report real screen coordinates.
            ext = screen_extents(acc)
            return int(ext.x + ext.width / 2), int(ext.y + ext.height / 2)
        # Wayland clients report surface-relative extents; add the window origin.
        ext = window_extents(acc)
        x, y = aw["at"]
        return int(x + ext.x + ext.width / 2), int(y + ext.y + ext.height / 2)

    def _btn_dev(self) -> UInput:
        if self._btn is None:
            cap = {
                e.EV_KEY: [e.BTN_LEFT, e.BTN_RIGHT, e.BTN_MIDDLE],
                e.EV_REL: [e.REL_X, e.REL_Y, e.REL_WHEEL],
            }
            self._btn = UInput(cap, name="desktop-mcp-pointer")
            time.sleep(0.3)  # let the compositor register the new device
        return self._btn

    def pointer(self, x: int, y: int, button: str = "left", clicks: int = 1,
                scroll: int = 0) -> None:
        # Hyprland places the cursor exactly; uinput only supplies buttons/wheel.
        _hyprctl_dispatch("movecursor", str(int(x)), str(int(y)))
        time.sleep(0.05)
        dev = self._btn_dev()
        btn = {"left": e.BTN_LEFT, "right": e.BTN_RIGHT, "middle": e.BTN_MIDDLE}[button]
        for _ in range(max(0, min(clicks, 3))):
            dev.write(e.EV_KEY, btn, 1); dev.syn()
            time.sleep(0.03)
            dev.write(e.EV_KEY, btn, 0); dev.syn()
            time.sleep(0.06)
        if scroll:
            dev.write(e.EV_REL, e.REL_WHEEL, -scroll); dev.syn()

    # ------------------------------------------------------------ windows

    def activate_window(self, query: str, allowed: set[str]) -> None:
        q = query.lower()
        for client in _hyprctl_json("clients") or []:
            if not client.get("mapped", True) or client.get("hidden"):
                continue
            app = self._client_app_name(client)
            if app not in allowed:
                continue
            title = (client.get("title") or "").lower()
            cls = (client.get("class") or "").lower()
            if q in title or q in app or q in cls:
                _hyprctl_dispatch("focuswindow", f"address:{client['address']}")
                time.sleep(0.2)
                return

    def screenshot(self) -> str | None:
        aw = self._active_window()
        if not aw or not shutil.which("grim"):
            return None
        (x, y), (w, h) = aw["at"], aw["size"]
        fd, path = tempfile.mkstemp(prefix="desktop-mcp-", suffix=".png")
        os.close(fd)
        out = subprocess.run(["grim", "-g", f"{x},{y} {w}x{h}", path],
                             capture_output=True, timeout=15)
        return path if out.returncode == 0 else None

    # ------------------------------------------------------------ text

    def type_text(self, text: str) -> None:
        # wtype uses the virtual-keyboard protocol: Unicode-safe and leaves the
        # clipboard alone. Without it, fall back to paste.
        if not shutil.which("wtype"):
            super().type_text(text)
            return
        text = text.replace("\r\n", "\n").replace("\r", "\n")
        for cmd in _wtype_commands(text):
            subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           timeout=10, check=True)


def _wtype_commands(text: str) -> list[list[str]]:
    """Split text so newlines and tabs are sent as real Return/Tab keys."""
    cmds: list[list[str]] = []
    chunk = ""
    for ch in text:
        if ch in "\n\t":
            if chunk:
                cmds.append(["wtype", "--", chunk])
                chunk = ""
            cmds.append(["wtype", "-k", "Return" if ch == "\n" else "Tab"])
        else:
            chunk += ch
    if chunk:
        cmds.append(["wtype", "--", chunk])
    return cmds
