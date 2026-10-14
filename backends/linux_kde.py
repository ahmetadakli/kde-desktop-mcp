"""KDE Plasma (Wayland) backend: AT-SPI, uinput and KWin scripting."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, Gio, GLib  # noqa: E402

from evdev import UInput, ecodes as e, AbsInfo  # noqa: E402

from .base import AppInfo, Backend, Node, WindowInfo  # noqa: E402

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


class AtspiNode(Node):
    def __init__(self, acc: Atspi.Accessible):
        self.acc = acc

    def role(self) -> str:
        return self.acc.get_role_name() or ""

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


ABS_MAX = 32767

ALIASES = {
    "ctrl": "LEFTCTRL", "control": "LEFTCTRL", "shift": "LEFTSHIFT",
    "alt": "LEFTALT", "altgr": "RIGHTALT", "super": "LEFTMETA", "meta": "LEFTMETA",
    "win": "LEFTMETA", "enter": "ENTER", "return": "ENTER", "esc": "ESC",
    "escape": "ESC", "del": "DELETE", "pgup": "PAGEUP", "pgdn": "PAGEDOWN",
    "space": "SPACE", "tab": "TAB", "backspace": "BACKSPACE",
}

KWIN_ACTIVATE = """
const allow = %s;
const q = %s.toLowerCase();
for (const w of workspace.windowList()) {
  const cls = (w.resourceClass || "").toLowerCase();
  const name = (w.resourceName || "").toLowerCase();
  const okApp = allow.some(a => cls.includes(a) || name.includes(a));
  if (okApp && (w.caption.toLowerCase().includes(q) || cls.includes(q))) {
    workspace.activeWindow = w;
    break;
  }
}
"""

_SINK_XML = (
    "<node><interface name='org.desktopmcp.Sink'>"
    "<method name='Report'><arg type='s' name='data' direction='in'/></method>"
    "</interface></node>")


class KdeBackend(Backend):
    platform = "linux-kde"
    allow_file = Path.home() / ".config" / "desktop-mcp" / "allow.txt"
    default_allow = DEFAULT_ALLOW
    boring_roles = {"filler", "panel", "section", "unknown", "redundant object", "grouping"}
    apps_note = ("Apps started before accessibility was enabled may need a restart "
                 "(Chromium/Electron: --force-renderer-accessibility).")

    def __init__(self) -> None:
        self._kbd: UInput | None = None
        self._ptr: UInput | None = None

    # ------------------------------------------------------------ reading

    def enable(self) -> None:
        subprocess.run(
            ["gdbus", "call", "--session", "--dest", "org.a11y.Bus",
             "--object-path", "/org/a11y/bus", "--method",
             "org.freedesktop.DBus.Properties.Set", "org.a11y.Status",
             "IsEnabled", "<true>"],
            capture_output=True, timeout=5,
        )

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
                active = w.get_state_set().contains(Atspi.StateType.ACTIVE)
                info.windows.append(
                    WindowInfo(w.get_role_name() or "", (w.get_name() or "").strip(), active))
            out.append(info)
        return out

    def app_root(self, name: str) -> Node | None:
        for a in self._applications():
            if (a.get_name() or "").lower() == name.lower():
                return AtspiNode(a)
        return None

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

    def element_center(self, node: Node) -> tuple[int, int]:
        # On Wayland AT-SPI extents are relative to the toplevel surface, so add
        # the active window's client origin. The element's window must be active.
        acc = node.acc
        frame = _frame_of(acc)
        if frame is None or not frame.get_state_set().contains(Atspi.StateType.ACTIVE):
            raise RuntimeError("element's window is not active; call activate_window first")
        ext = Atspi.Component.get_extents(acc, Atspi.CoordType.WINDOW)
        if ext.width <= 0 or ext.height <= 0:
            raise RuntimeError("element has no on-screen area")
        geo = self._active_window_geometry()
        return int(geo["x"] + ext.x + ext.width / 2), int(geo["y"] + ext.y + ext.height / 2)

    # ------------------------------------------------------------ input

    def _kbd_dev(self) -> UInput:
        if self._kbd is None:
            keys = [v for k, v in e.ecodes.items() if k.startswith("KEY_") and v < 0x200]
            self._kbd = UInput({e.EV_KEY: keys}, name="desktop-mcp-keyboard")
            time.sleep(0.3)  # let the compositor register the new device
        return self._kbd

    def _ptr_dev(self) -> UInput:
        if self._ptr is None:
            cap = {
                e.EV_KEY: [e.BTN_LEFT, e.BTN_RIGHT, e.BTN_MIDDLE],
                e.EV_ABS: [
                    (e.ABS_X, AbsInfo(0, 0, ABS_MAX, 0, 0, 0)),
                    (e.ABS_Y, AbsInfo(0, 0, ABS_MAX, 0, 0, 0)),
                ],
                e.EV_REL: [e.REL_WHEEL],
            }
            self._ptr = UInput(cap, name="desktop-mcp-pointer")
            time.sleep(0.3)
        return self._ptr

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

    def pointer(self, x: int, y: int, button: str = "left", clicks: int = 1,
                scroll: int = 0) -> None:
        x0, y0, x1, y1 = _screen_box()
        ax = int((x - x0) * ABS_MAX / max(1, x1 - x0))
        ay = int((y - y0) * ABS_MAX / max(1, y1 - y0))
        dev = self._ptr_dev()
        dev.write(e.EV_ABS, e.ABS_X, ax)
        dev.write(e.EV_ABS, e.ABS_Y, ay)
        dev.syn()
        time.sleep(0.05)
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
        _run_kwin_script(KWIN_ACTIVATE % (json.dumps(sorted(allowed)), json.dumps(query)))

    def screenshot(self) -> str | None:
        path = Path(tempfile.gettempdir()) / f"desktop-mcp-{time.time_ns()}.png"
        subprocess.run(["spectacle", "-b", "-n", "-a", "-o", str(path)],
                       capture_output=True, timeout=15)
        return str(path) if path.exists() else None

    def _active_window_geometry(self) -> dict:
        """Client-area geometry (logical px) and identity of KWin's active window."""
        raw = _kwin_query(
            "JSON.stringify((() => { const w = workspace.activeWindow; if (!w) return null;"
            " const g = w.clientGeometry; return {x: g.x, y: g.y, width: g.width,"
            " height: g.height, caption: w.caption, cls: w.resourceClass}; })())")
        if not raw or raw == "null":
            raise RuntimeError("could not read the active window geometry from KWin")
        return json.loads(raw)


def _frame_of(acc: Atspi.Accessible) -> Atspi.Accessible | None:
    cur = acc
    while cur is not None:
        if cur.get_role_name() in ("frame", "window", "dialog"):
            return cur
        cur = cur.get_parent()
    return None


def _screen_box() -> tuple[int, int, int, int]:
    try:
        out = subprocess.run(["kscreen-doctor", "-j"], capture_output=True,
                             text=True, timeout=5).stdout
        xs, ys, xe, ye = [], [], [], []
        for o in json.loads(out)["outputs"]:
            if not o.get("enabled"):
                continue
            sc = o.get("scale") or 1
            w, h = o["size"]["width"] / sc, o["size"]["height"] / sc
            xs.append(o["pos"]["x"]); ys.append(o["pos"]["y"])
            xe.append(o["pos"]["x"] + w); ye.append(o["pos"]["y"] + h)
        return int(min(xs)), int(min(ys)), int(max(xe)), int(max(ye))
    except Exception:
        return 0, 0, 1920, 1080


def _run_kwin_script(script: str, until=None, timeout: float = 0.2) -> None:
    """Load, run and unload a KWin script. Keeps it loaded until `until()` is
    true (pumping the GLib main context) or `timeout` seconds pass."""
    with tempfile.NamedTemporaryFile("w", suffix=".js", delete=False) as f:
        f.write(script)
        path = f.name
    plugin = f"desktop-mcp-{os.getpid()}-{time.time_ns()}"
    try:
        out = subprocess.run(
            ["qdbus6", "org.kde.KWin", "/Scripting",
             "org.kde.kwin.Scripting.loadScript", path, plugin],
            capture_output=True, text=True, timeout=5)
        sid = out.stdout.strip()
        if not sid.lstrip("-").isdigit() or int(sid) < 0:
            raise RuntimeError(f"KWin refused script: {out.stderr.strip()}")
        subprocess.run(["qdbus6", "org.kde.KWin", f"/Scripting/Script{sid}",
                        "org.kde.kwin.Script.run"], capture_output=True, timeout=5)
        ctx = GLib.MainContext.default()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline and not (until and until()):
            if not ctx.iteration(False):
                time.sleep(0.01)
        subprocess.run(["qdbus6", "org.kde.KWin", "/Scripting",
                        "org.kde.kwin.Scripting.unloadScript", plugin],
                       capture_output=True, timeout=5)
    finally:
        os.unlink(path)


def _kwin_query(js_expr: str, timeout: float = 3.0) -> str | None:
    """Evaluate a JS expression inside KWin and return its string value.

    KWin scripts cannot return values over D-Bus, so the script calls back into
    a temporary object exported on this process's unique bus name.
    """
    conn = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    box: dict[str, str] = {}

    def on_call(_conn, _sender, _path, _iface, _method, params, inv):
        box["data"] = params.unpack()[0]
        inv.return_value(None)

    path = f"/sink{time.time_ns()}"
    node = Gio.DBusNodeInfo.new_for_xml(_SINK_XML)
    reg = conn.register_object(path, node.interfaces[0], on_call, None, None)
    try:
        script = (f"const result = String({js_expr});\n"
                  f"callDBus({json.dumps(conn.get_unique_name())}, {json.dumps(path)}, "
                  f"'org.desktopmcp.Sink', 'Report', result);\n")
        _run_kwin_script(script, until=lambda: "data" in box, timeout=timeout)
    finally:
        conn.unregister_object(reg)
    return box.get("data")
