"""KDE Plasma (Wayland) backend: KWin scripting for windows, absolute uinput pointer."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

from evdev import UInput, ecodes as e, AbsInfo

from .base import Node
from .linux_common import LinuxAtspiBackend, frame_of, window_extents  # sets gi versions

from gi.repository import Atspi, Gio, GLib  # noqa: E402

ABS_MAX = 32767

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


class KdeBackend(LinuxAtspiBackend):
    platform = "linux-kde"

    def __init__(self) -> None:
        super().__init__()
        self._ptr: UInput | None = None

    def element_center(self, node: Node) -> tuple[int, int]:
        # On Wayland AT-SPI extents are relative to the toplevel surface, so add
        # the active window's client origin. The element's window must be active.
        acc = node.acc
        frame = frame_of(acc)
        if frame is None or not frame.get_state_set().contains(Atspi.StateType.ACTIVE):
            raise RuntimeError("element's window is not active; call activate_window first")
        ext = window_extents(acc)
        geo = self._active_window_geometry()
        return int(geo["x"] + ext.x + ext.width / 2), int(geo["y"] + ext.y + ext.height / 2)

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
