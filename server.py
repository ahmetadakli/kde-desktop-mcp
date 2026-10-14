"""Desktop control MCP server for KDE Plasma (Wayland).

Reads and drives applications through the AT-SPI accessibility tree instead of
screenshots. Keyboard and pointer input go through a uinput virtual device.
Every app-scoped operation is restricted to the allowlist in
~/.config/desktop-mcp/allow.txt (re-read on every call).
"""

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
from mcp.server.fastmcp import FastMCP  # noqa: E402

CONFIG_DIR = Path.home() / ".config" / "desktop-mcp"
ALLOW_FILE = CONFIG_DIR / "allow.txt"
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

mcp = FastMCP("desktop")

# ---------------------------------------------------------------- allowlist


def allowed() -> set[str]:
    if not ALLOW_FILE.exists():
        CONFIG_DIR.mkdir(parents=True, exist_ok=True)
        ALLOW_FILE.write_text(DEFAULT_ALLOW)
    names = set()
    for line in ALLOW_FILE.read_text().splitlines():
        line = line.split("#", 1)[0].strip().lower()
        if line:
            names.add(line)
    return names


def is_allowed(app_name: str) -> bool:
    return (app_name or "").strip().lower() in allowed()


# ---------------------------------------------------------------- AT-SPI


def enable_a11y() -> None:
    subprocess.run(
        ["gdbus", "call", "--session", "--dest", "org.a11y.Bus",
         "--object-path", "/org/a11y/bus", "--method",
         "org.freedesktop.DBus.Properties.Set", "org.a11y.Status",
         "IsEnabled", "<true>"],
        capture_output=True, timeout=5,
    )


_refs: dict[int, Atspi.Accessible] = {}
_next_id = 1


def ref(acc: Atspi.Accessible) -> int:
    global _next_id
    if len(_refs) > 20000:
        _refs.clear()
    rid = _next_id
    _next_id += 1
    _refs[rid] = acc
    return rid


def app_of(acc: Atspi.Accessible) -> str:
    try:
        return acc.get_application().get_name() or ""
    except Exception:
        return ""


def resolve(rid: int) -> Atspi.Accessible:
    acc = _refs.get(rid)
    if acc is None:
        raise ValueError(f"unknown id {rid}; call tree/find again")
    if not is_allowed(app_of(acc)):
        raise PermissionError(f"app '{app_of(acc)}' is not in {ALLOW_FILE}")
    return acc


def applications() -> list[Atspi.Accessible]:
    desktop = Atspi.get_desktop(0)
    out = []
    for i in range(desktop.get_child_count()):
        a = desktop.get_child_at_index(i)
        if a is not None:
            out.append(a)
    return out


def find_app(name: str) -> Atspi.Accessible:
    if not is_allowed(name):
        raise PermissionError(f"app '{name}' is not in {ALLOW_FILE}")
    for a in applications():
        if (a.get_name() or "").lower() == name.lower():
            return a
    raise ValueError(f"app '{name}' not found on the accessibility bus")


def states(acc: Atspi.Accessible) -> list[str]:
    try:
        ss = acc.get_state_set()
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


def actions(acc: Atspi.Accessible) -> list[str]:
    try:
        n = acc.get_n_actions()
        return [acc.get_action_name(i) for i in range(n)]
    except Exception:
        return []


def showing(acc: Atspi.Accessible) -> bool:
    try:
        return acc.get_state_set().contains(Atspi.StateType.SHOWING)
    except Exception:
        return False


def text_of(acc: Atspi.Accessible, limit: int = 80) -> str:
    try:
        if acc.get_text_iface() is None:
            return ""
        s = Atspi.Text.get_text(acc, 0, min(Atspi.Text.get_character_count(acc), limit))
        return s.replace("\n", " ")
    except Exception:
        return ""


def describe(acc: Atspi.Accessible) -> str:
    rid = ref(acc)
    role = acc.get_role_name() or "?"
    name = (acc.get_name() or "").replace("\n", " ")[:80]
    parts = [f"[{rid}] {role}"]
    if name:
        parts.append(json.dumps(name, ensure_ascii=False))
    txt = text_of(acc)
    if txt and txt != name:
        parts.append("text=" + json.dumps(txt, ensure_ascii=False))
    st = states(acc)
    if st:
        parts.append("{" + ",".join(st) + "}")
    act = actions(acc)
    if act:
        parts.append("actions=" + ",".join(a for a in act if a))
    return " ".join(parts)


BORING = {"filler", "panel", "section", "unknown", "redundant object", "grouping"}


def walk(acc, depth, max_depth, lines, budget, only_showing=True):
    if len(lines) >= budget or depth > max_depth:
        return
    try:
        n = acc.get_child_count()
    except Exception:
        return
    for i in range(n):
        if len(lines) >= budget:
            lines.append("… truncated; narrow with find or a deeper root id")
            return
        try:
            c = acc.get_child_at_index(i)
        except Exception:
            continue
        if c is None or (only_showing and not showing(c)):
            continue
        role = c.get_role_name() or ""
        interesting = (role not in BORING) or bool(c.get_name()) or actions(c)
        if interesting:
            lines.append("  " * depth + describe(c))
            walk(c, depth + 1, max_depth, lines, budget, only_showing)
        else:
            walk(c, depth, max_depth, lines, budget, only_showing)


# ---------------------------------------------------------------- input


def screen_box() -> tuple[int, int, int, int]:
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


ABS_MAX = 32767
_kbd: UInput | None = None
_ptr: UInput | None = None


def kbd() -> UInput:
    global _kbd
    if _kbd is None:
        keys = [v for k, v in e.ecodes.items() if k.startswith("KEY_") and v < 0x200]
        _kbd = UInput({e.EV_KEY: keys}, name="desktop-mcp-keyboard")
        time.sleep(0.3)  # let the compositor register the new device
    return _kbd


def ptr() -> UInput:
    global _ptr
    if _ptr is None:
        cap = {
            e.EV_KEY: [e.BTN_LEFT, e.BTN_RIGHT, e.BTN_MIDDLE],
            e.EV_ABS: [
                (e.ABS_X, AbsInfo(0, 0, ABS_MAX, 0, 0, 0)),
                (e.ABS_Y, AbsInfo(0, 0, ABS_MAX, 0, 0, 0)),
            ],
            e.EV_REL: [e.REL_WHEEL],
        }
        _ptr = UInput(cap, name="desktop-mcp-pointer")
        time.sleep(0.3)
    return _ptr


ALIASES = {
    "ctrl": "LEFTCTRL", "control": "LEFTCTRL", "shift": "LEFTSHIFT",
    "alt": "LEFTALT", "altgr": "RIGHTALT", "super": "LEFTMETA", "meta": "LEFTMETA",
    "win": "LEFTMETA", "enter": "ENTER", "return": "ENTER", "esc": "ESC",
    "escape": "ESC", "del": "DELETE", "pgup": "PAGEUP", "pgdn": "PAGEDOWN",
    "space": "SPACE", "tab": "TAB", "backspace": "BACKSPACE",
}


def keycode(name: str) -> int:
    n = ALIASES.get(name.lower(), name.upper())
    code = e.ecodes.get("KEY_" + n)
    if code is None:
        raise ValueError(f"unknown key '{name}'")
    return code


def active_app() -> str:
    for a in applications():
        try:
            for i in range(a.get_child_count()):
                w = a.get_child_at_index(i)
                if w is not None and w.get_state_set().contains(Atspi.StateType.ACTIVE):
                    return a.get_name() or ""
        except Exception:
            continue
    return ""


def require_active_allowed() -> str:
    name = active_app()
    if not is_allowed(name):
        raise PermissionError(
            f"active window app '{name or 'unknown'}' is not in {ALLOW_FILE}; "
            "activate an allowed window first")
    return name


def press_combo(combo: str) -> None:
    codes = [keycode(k) for k in combo.replace(" ", "").split("+") if k]
    dev = kbd()
    for c in codes:
        dev.write(e.EV_KEY, c, 1)
    dev.syn()
    time.sleep(0.02)
    for c in reversed(codes):
        dev.write(e.EV_KEY, c, 0)
    dev.syn()
    time.sleep(0.03)


# ---------------------------------------------------------------- tools


@mcp.tool()
def apps() -> str:
    """List applications on the accessibility bus with their top-level windows.
    Marks which are allowed and which window is active."""
    enable_a11y()
    allow = allowed()
    lines = []
    for a in applications():
        name = a.get_name() or ""
        if a.get_child_count() == 0:
            continue
        flag = "allowed" if name.lower() in allow else "blocked"
        lines.append(f"{name} ({flag})")
        for i in range(a.get_child_count()):
            w = a.get_child_at_index(i)
            if w is None:
                continue
            act = " [active]" if w.get_state_set().contains(Atspi.StateType.ACTIVE) else ""
            title = (w.get_name() or "").strip()
            lines.append(f"  - {w.get_role_name()}: {title!r}{act}")
    lines.append(f"allowlist file: {ALLOW_FILE}")
    lines.append("Apps started before accessibility was enabled may need a restart "
                 "(Chromium/Electron: --force-renderer-accessibility).")
    return "\n".join(lines)


@mcp.tool()
def tree(app: str = "", root_id: int = 0, max_depth: int = 12,
         max_nodes: int = 300, include_hidden: bool = False) -> str:
    """Return the accessibility tree as `[id] role "name" {states} actions=…`.
    Pass `app` (name from `apps`) for its windows, or `root_id` to expand a subtree.
    Ids are used by act/set_text/get_text/focus."""
    enable_a11y()
    if root_id:
        root = resolve(root_id)
        lines = [describe(root)]
    else:
        root = find_app(app)
        lines = [f"{root.get_name()}"]
    walk(root, 1, max_depth, lines, max_nodes, not include_hidden)
    return "\n".join(lines)


@mcp.tool()
def find(query: str, app: str, role: str = "", max_results: int = 30) -> str:
    """Search an allowed app for elements whose name, text or role contains `query`
    (case-insensitive). Optionally filter by exact role name (e.g. "push button")."""
    enable_a11y()
    root = find_app(app)
    q = query.lower()
    hits: list[str] = []
    stack = [root]
    seen = 0
    while stack and len(hits) < max_results and seen < 20000:
        acc = stack.pop()
        seen += 1
        try:
            n = acc.get_child_count()
        except Exception:
            continue
        for i in reversed(range(n)):
            try:
                c = acc.get_child_at_index(i)
            except Exception:
                continue
            if c is None or not showing(c):
                continue
            stack.append(c)
            r = c.get_role_name() or ""
            if role and r != role:
                continue
            hay = f"{c.get_name() or ''} {text_of(c, 200)} {r}".lower()
            if q in hay:
                hits.append(describe(c))
    return "\n".join(hits) or "no matches"


@mcp.tool()
def act(id: int, action: str = "") -> str:
    """Perform an accessibility action on an element (default: its first action,
    usually click/press/activate). Use the action names listed by tree/find."""
    acc = resolve(id)
    names = actions(acc)
    if not names:
        raise ValueError("element has no actions; try focus + key, or mouse")
    idx = 0
    if action:
        lowered = [n.lower() for n in names]
        if action.lower() not in lowered:
            raise ValueError(f"action '{action}' not in {names}")
        idx = lowered.index(action.lower())
    ok = acc.do_action(idx)
    return f"{names[idx]} -> {'ok' if ok else 'failed'}"


@mcp.tool()
def set_text(id: int, text: str) -> str:
    """Replace the contents of an editable text element."""
    acc = resolve(id)
    if acc.get_editable_text_iface() is None:
        raise ValueError("element is not editable text; use focus + type")
    ok = Atspi.EditableText.set_text_contents(acc, text)
    return "ok" if ok else "failed"


@mcp.tool()
def get_text(id: int, limit: int = 20000) -> str:
    """Read the full text of an element (documents, fields, labels)."""
    acc = resolve(id)
    if acc.get_text_iface() is None:
        return acc.get_name() or ""
    return Atspi.Text.get_text(acc, 0, min(Atspi.Text.get_character_count(acc), limit))


def run_kwin_script(script: str, until=None, timeout: float = 0.2) -> None:
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


_SINK_XML = (
    "<node><interface name='org.desktopmcp.Sink'>"
    "<method name='Report'><arg type='s' name='data' direction='in'/></method>"
    "</interface></node>")


def kwin_query(js_expr: str, timeout: float = 3.0) -> str | None:
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
        run_kwin_script(script, until=lambda: "data" in box, timeout=timeout)
    finally:
        conn.unregister_object(reg)
    return box.get("data")


def active_window_geometry() -> dict:
    """Client-area geometry (logical px) and identity of KWin's active window."""
    raw = kwin_query(
        "JSON.stringify((() => { const w = workspace.activeWindow; if (!w) return null;"
        " const g = w.clientGeometry; return {x: g.x, y: g.y, width: g.width,"
        " height: g.height, caption: w.caption, cls: w.resourceClass}; })())")
    if not raw or raw == "null":
        raise RuntimeError("could not read the active window geometry from KWin")
    return json.loads(raw)


def frame_of(acc: Atspi.Accessible) -> Atspi.Accessible | None:
    cur = acc
    while cur is not None:
        if cur.get_role_name() in ("frame", "window", "dialog"):
            return cur
        cur = cur.get_parent()
    return None


def element_center(acc: Atspi.Accessible) -> tuple[int, int]:
    """Screen position of an element's centre.

    On Wayland AT-SPI extents are relative to the toplevel surface, so add the
    active window's client origin. The element's window must be the active one.
    """
    frame = frame_of(acc)
    if frame is None or not frame.get_state_set().contains(Atspi.StateType.ACTIVE):
        raise RuntimeError("element's window is not active; call activate_window first")
    ext = Atspi.Component.get_extents(acc, Atspi.CoordType.WINDOW)
    if ext.width <= 0 or ext.height <= 0:
        raise RuntimeError("element has no on-screen area")
    geo = active_window_geometry()
    return int(geo["x"] + ext.x + ext.width / 2), int(geo["y"] + ext.y + ext.height / 2)


def pointer_click(x: int, y: int, button: str = "left", clicks: int = 1,
                  scroll: int = 0) -> None:
    x0, y0, x1, y1 = screen_box()
    ax = int((x - x0) * ABS_MAX / max(1, x1 - x0))
    ay = int((y - y0) * ABS_MAX / max(1, y1 - y0))
    dev = ptr()
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


def is_focused(acc: Atspi.Accessible) -> bool:
    try:
        return acc.get_state_set().contains(Atspi.StateType.FOCUSED)
    except Exception:
        return False


@mcp.tool()
def focus(id: int) -> str:
    """Move keyboard focus to an element. Tries the accessibility focus request,
    then a "Set Focus" action, then a real click on the element's centre (the
    element's window must be active). Reports which method worked."""
    acc = resolve(id)
    if acc.get_component_iface() is None:
        raise ValueError("element cannot take focus")
    if Atspi.Component.grab_focus(acc):
        time.sleep(0.05)
        if is_focused(acc):
            return "ok (grab_focus)"
    names = [n.lower().replace(" ", "") for n in actions(acc)]
    if "setfocus" in names:
        acc.do_action(names.index("setfocus"))
        time.sleep(0.05)
        if is_focused(acc):
            return "ok (set focus action)"
    require_active_allowed()
    x, y = element_center(acc)
    pointer_click(x, y)
    time.sleep(0.15)
    if is_focused(acc):
        return f"ok (clicked at {x},{y})"
    return f"clicked at {x},{y} but element did not report focus"


@mcp.tool()
def click(id: int, button: str = "left", clicks: int = 1) -> str:
    """Real pointer click on an element's centre, for elements without actions.
    The element's window must be active."""
    acc = resolve(id)
    require_active_allowed()
    x, y = element_center(acc)
    pointer_click(x, y, button, clicks)
    return f"clicked {button} x{clicks} at {x},{y}"


@mcp.tool()
def key(combo: str, repeat: int = 1) -> str:
    """Press a key or chord in the active window, e.g. "ctrl+s", "alt+tab", "enter",
    "f5". Key names follow evdev (KEY_*) on the physical US layout positions.
    Only sent when the active window belongs to an allowed app."""
    app = require_active_allowed()
    for _ in range(max(1, min(repeat, 100))):
        press_combo(combo)
    return f"sent {combo} x{repeat} to {app}"


@mcp.tool()
def type_text(text: str) -> str:
    """Type text into the focused element of the active window by pasting via the
    clipboard (works with any keyboard layout and Unicode). Overwrites the clipboard."""
    app = require_active_allowed()
    subprocess.run(["wl-copy"], input=text.encode(), stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL, timeout=5, check=True)
    time.sleep(0.1)
    press_combo("ctrl+v")
    return f"typed {len(text)} chars into {app}"


@mcp.tool()
def mouse(x: int, y: int, button: str = "left", clicks: int = 1,
          scroll: int = 0) -> str:
    """Fallback pointer input in logical screen coordinates: move, then click
    (`clicks`=0 to only move) and/or scroll (positive = down)."""
    app = require_active_allowed()
    pointer_click(x, y, button, clicks, scroll)
    return f"pointer at ({x},{y}) in {app}"


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


@mcp.tool()
def activate_window(query: str) -> str:
    """Bring an allowed app's window to the front by caption or app-class substring."""
    run_kwin_script(KWIN_ACTIVATE % (json.dumps(sorted(allowed())), json.dumps(query)))
    return f"active app now: {active_app() or 'unknown'}"


@mcp.tool()
def screenshot() -> str:
    """Last resort only: capture the active window to a PNG and return its path
    (read it with the Read tool). Prefer tree/find."""
    require_active_allowed()
    path = Path(tempfile.gettempdir()) / f"desktop-mcp-{time.time_ns()}.png"
    subprocess.run(["spectacle", "-b", "-n", "-a", "-o", str(path)],
                   capture_output=True, timeout=15)
    return str(path) if path.exists() else "screenshot failed"


if __name__ == "__main__":
    enable_a11y()
    allowed()
    mcp.run()
