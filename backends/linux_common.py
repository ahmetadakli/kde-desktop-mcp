"""Compositor-independent Linux pieces: AT-SPI reading and uinput keyboard.

Compositor backends (KDE, Hyprland) subclass LinuxAtspiBackend and provide
window management, element geometry, pointer input and screenshots.
"""

from __future__ import annotations

import subprocess
import threading
import time
from pathlib import Path

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, Gio, GLib  # noqa: E402

from evdev import UInput, ecodes as e  # noqa: E402

from .base import AppGroupNode, AppInfo, Backend, Node, WindowInfo  # noqa: E402

DEFAULT_ALLOW = """\
# One AT-SPI application name per line (case-insensitive). See `apps` tool output.
# Keyboard/mouse input is only sent when the active window belongs to one of these.
firefox
chromium
google-chrome
dolphin
kate
okular
gwenview
soffice
"""


class FastUInput(UInput):
    """UInput that skips evdev's lookup of the new /dev/input/event* node.

    We only write to the uinput fd. Opening the event node needs the `input`
    group; without it evdev retries for two seconds on every device creation.
    """

    def _find_device(self, fd: int) -> None:
        return None


class AtspiNode(Node):
    def __init__(self, acc: Atspi.Accessible):
        self.acc = acc
        self._role: str | None = None

    def role(self) -> str:
        # Roles never change; tree rendering asks for them twice per node.
        if self._role is None:
            self._role = self.acc.get_role_name() or ""
        return self._role

    def name(self) -> str:
        return self.acc.get_name() or ""

    def text(self, limit: int = 80) -> str:
        try:
            if self.acc.get_text_iface() is None:
                return ""
            n = min(Atspi.Text.get_character_count(self.acc), limit)
            return Atspi.Text.get_text(self.acc, 0, n).replace("\n", " ")
        except Exception:
            return ""

    def full_text(self, limit: int) -> str:
        if self.acc.get_text_iface() is None:
            return self.name()
        n = min(Atspi.Text.get_character_count(self.acc), limit)
        return Atspi.Text.get_text(self.acc, 0, n)

    def states(self) -> list[str]:
        try:
            ss = self.acc.get_state_set()
        except Exception:
            return []
        wanted = {
            Atspi.StateType.FOCUSED: "focused",
            Atspi.StateType.CHECKED: "checked",
            Atspi.StateType.SELECTED: "selected",
            Atspi.StateType.EXPANDED: "expanded",
            Atspi.StateType.ACTIVE: "active",
            Atspi.StateType.EDITABLE: "editable",
        }
        out = [label for st, label in wanted.items() if ss.contains(st)]
        if not ss.contains(Atspi.StateType.ENABLED):
            out.append("disabled")
        return out

    def actions(self) -> list[str]:
        try:
            n = self.acc.get_n_actions()
            return [self.acc.get_action_name(i) for i in range(n)]
        except Exception:
            return []

    def do_action(self, index: int) -> bool:
        return bool(self.acc.do_action(index))

    def set_text(self, text: str) -> bool:
        if self.acc.get_editable_text_iface() is None:
            raise ValueError("element is not editable text; use focus + type")
        return bool(Atspi.EditableText.set_text_contents(self.acc, text))

    def children(self) -> list[Node]:
        out: list[Node] = []
        try:
            n = self.acc.get_child_count()
        except Exception:
            return out
        for i in range(n):
            try:
                c = self.acc.get_child_at_index(i)
            except Exception:
                continue
            if c is not None:
                out.append(AtspiNode(c))
        return out

    def showing(self) -> bool:
        try:
            return self.acc.get_state_set().contains(Atspi.StateType.SHOWING)
        except Exception:
            return False

    def app_name(self) -> str:
        try:
            return self.acc.get_application().get_name() or ""
        except Exception:
            return ""

    def is_focused(self) -> bool:
        try:
            return self.acc.get_state_set().contains(Atspi.StateType.FOCUSED)
        except Exception:
            return False


ALIASES = {
    "ctrl": "LEFTCTRL", "control": "LEFTCTRL", "shift": "LEFTSHIFT",
    "alt": "LEFTALT", "altgr": "RIGHTALT", "super": "LEFTMETA", "meta": "LEFTMETA",
    "win": "LEFTMETA", "enter": "ENTER", "return": "ENTER", "esc": "ESC",
    "escape": "ESC", "del": "DELETE", "pgup": "PAGEUP", "pgdn": "PAGEDOWN",
    "space": "SPACE", "tab": "TAB", "backspace": "BACKSPACE",
}


def frame_of(acc: Atspi.Accessible) -> Atspi.Accessible | None:
    cur = acc
    while cur is not None:
        if cur.get_role_name() in ("frame", "window", "dialog"):
            return cur
        cur = cur.get_parent()
    return None


def window_extents(acc: Atspi.Accessible):
    """Element extents relative to its toplevel surface (what Wayland exposes)."""
    ext = Atspi.Component.get_extents(acc, Atspi.CoordType.WINDOW)
    if ext.width <= 0 or ext.height <= 0:
        raise RuntimeError("element has no on-screen area")
    return ext


class LinuxAtspiBackend(Backend):
    allow_file = Path.home() / ".config" / "desktop-mcp" / "allow.txt"
    default_allow = DEFAULT_ALLOW
    boring_roles = {"filler", "panel", "section", "unknown", "redundant object", "grouping"}
    apps_note = ("Apps started before accessibility was enabled may need a restart "
                 "(Chromium/Electron: --force-renderer-accessibility).")

    def __init__(self) -> None:
        self._kbd: UInput | None = None
        self._dev_lock = threading.Lock()

    def warm_up(self) -> None:
        self._kbd_dev()

    # ------------------------------------------------------------ reading

    def enable(self) -> None:
        try:
            Gio.bus_get_sync(Gio.BusType.SESSION, None).call_sync(
                "org.a11y.Bus", "/org/a11y/bus", "org.freedesktop.DBus.Properties",
                "Set", GLib.Variant("(ssv)", ("org.a11y.Status", "IsEnabled",
                                              GLib.Variant("b", True))),
                None, Gio.DBusCallFlags.NONE, 5000, None)
        except GLib.Error:
            pass

    def _applications(self) -> list[Atspi.Accessible]:
        desktop = Atspi.get_desktop(0)
        out = []
        for i in range(desktop.get_child_count()):
            a = desktop.get_child_at_index(i)
            if a is not None:
                out.append(a)
        return out

    def list_apps(self) -> list[AppInfo]:
        out = []
        for a in self._applications():
            info = AppInfo(a.get_name() or "")
            for i in range(a.get_child_count()):
                w = a.get_child_at_index(i)
                if w is None:
                    continue
                info.windows.append(WindowInfo(w.get_role_name() or "",
                                               (w.get_name() or "").strip(),
                                               self._window_active(a, w)))
            out.append(info)
        return out

    def _window_active(self, app: Atspi.Accessible, window: Atspi.Accessible) -> bool:
        return window.get_state_set().contains(Atspi.StateType.ACTIVE)

    def app_root(self, name: str) -> Node | None:
        # Every `kate -n` (and similar) is its own process with the same app name.
        def windows() -> list[Node]:
            out: list[Node] = []
            for a in self._applications():
                if (a.get_name() or "").lower() == name.lower():
                    out.extend(AtspiNode(a).children())
            return out

        root = AppGroupNode(name.lower(), windows)
        return root if any((a.get_name() or "").lower() == name.lower()
                           for a in self._applications()) else None

    def active_app(self) -> str:
        for a in self._applications():
            try:
                for i in range(a.get_child_count()):
                    w = a.get_child_at_index(i)
                    if w is not None and w.get_state_set().contains(Atspi.StateType.ACTIVE):
                        return a.get_name() or ""
            except Exception:
                continue
        return ""

    # ------------------------------------------------------------ focus

    def try_focus(self, node: Node) -> str | None:
        # Qt only honours GrabFocus when the element exposes a "Set Focus"
        # action, so try both before the caller falls back to a click.
        acc = node.acc
        if acc.get_component_iface() is None:
            raise ValueError("element cannot take focus")
        if Atspi.Component.grab_focus(acc):
            time.sleep(0.05)
            if node.is_focused():
                return "grab_focus"
        names = [n.lower().replace(" ", "") for n in node.actions()]
        if "setfocus" in names:
            acc.do_action(names.index("setfocus"))
            time.sleep(0.05)
            if node.is_focused():
                return "set focus action"
        return None

    # ------------------------------------------------------------ keyboard

    def _kbd_dev(self) -> UInput:
        with self._dev_lock:
            if self._kbd is None:
                keys = [v for k, v in e.ecodes.items() if k.startswith("KEY_") and v < 0x200]
                kbd = FastUInput({e.EV_KEY: keys}, name="desktop-mcp-keyboard")
                time.sleep(0.3)  # let the compositor register the new device
                self._kbd = kbd
        return self._kbd

    @staticmethod
    def _keycode(name: str) -> int:
        n = ALIASES.get(name.lower(), name.upper())
        code = e.ecodes.get("KEY_" + n)
        if code is None:
            raise ValueError(f"unknown key '{name}'")
        return code

    def press_combo(self, combo: str) -> None:
        codes = [self._keycode(k) for k in combo.replace(" ", "").split("+") if k]
        dev = self._kbd_dev()
        for c in codes:
            dev.write(e.EV_KEY, c, 1)
        dev.syn()
        time.sleep(0.02)
        for c in reversed(codes):
            dev.write(e.EV_KEY, c, 0)
        dev.syn()
        time.sleep(0.03)

    def type_text(self, text: str) -> None:
        # Paste instead of typing key by key: works with any layout and Unicode.
        subprocess.run(["wl-copy"], input=text.encode(), stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=5, check=True)
        time.sleep(0.1)
        self.press_combo("ctrl+v")
