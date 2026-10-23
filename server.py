"""Screenshot-free desktop control MCP server.

Reads and drives applications through the platform accessibility tree (AT-SPI on
KDE Plasma Wayland, UI Automation on Windows). Keyboard and pointer input are
synthesised by the platform backend. Every app-scoped operation is restricted
to the allowlist file (re-read on every call).
"""

from __future__ import annotations

import json
import time

from mcp.server.fastmcp import FastMCP

from backends import load_backend
from backends.base import Node

backend = load_backend()
ALLOW_FILE = backend.allow_file

mcp = FastMCP("desktop")

# ---------------------------------------------------------------- allowlist


def allowed() -> set[str]:
    if not ALLOW_FILE.exists():
        ALLOW_FILE.parent.mkdir(parents=True, exist_ok=True)
        ALLOW_FILE.write_text(backend.default_allow, encoding="utf-8")
    names = set()
    for line in ALLOW_FILE.read_text(encoding="utf-8").splitlines():
        line = line.split("#", 1)[0].strip().lower()
        if line:
            names.add(line)
    return names


def is_allowed(app_name: str) -> bool:
    return (app_name or "").strip().lower() in allowed()


def require_active_allowed() -> str:
    name = backend.active_app()
    if not is_allowed(name):
        raise PermissionError(
            f"active window app '{name or 'unknown'}' is not in {ALLOW_FILE}; "
            "activate an allowed window first")
    return name


# ---------------------------------------------------------------- ids

_refs: dict[int, Node] = {}
_next_id = 1


def ref(node: Node) -> int:
    global _next_id
    if len(_refs) > 20000:
        _refs.clear()
    rid = _next_id
    _next_id += 1
    _refs[rid] = node
    return rid


def resolve(rid: int) -> Node:
    node = _refs.get(rid)
    if node is None:
        raise ValueError(f"unknown id {rid}; call tree/find again")
    if not is_allowed(node.app_name()):
        raise PermissionError(f"app '{node.app_name()}' is not in {ALLOW_FILE}")
    return node


def find_app(name: str) -> Node:
    if not is_allowed(name):
        raise PermissionError(f"app '{name}' is not in {ALLOW_FILE}")
    root = backend.app_root(name)
    if root is None:
        raise ValueError(f"app '{name}' not found in the accessibility tree")
    return root


# ---------------------------------------------------------------- rendering

TRUNCATED = "… truncated; narrow with find or a deeper root id"


def describe(node: Node) -> str:
    rid = ref(node)
    name = node.name().replace("\n", " ")[:80]
    parts = [f"[{rid}] {node.role() or '?'}"]
    if name:
        parts.append(json.dumps(name, ensure_ascii=False))
    txt = node.text()
    if txt and txt != name:
        parts.append("text=" + json.dumps(txt, ensure_ascii=False))
    st = node.states()
    if st:
        parts.append("{" + ",".join(st) + "}")
    act = node.actions()
    if act:
        parts.append("actions=" + ",".join(a for a in act if a))
    return " ".join(parts)


def walk(node: Node, depth: int, max_depth: int, lines: list[str], budget: int,
         only_showing: bool = True) -> None:
    if len(lines) >= budget or depth > max_depth:
        return
    for c in node.children():
        if len(lines) >= budget:
            if lines[-1] != TRUNCATED:  # once, not once per open level
                lines.append(TRUNCATED)
            return
        if only_showing and not c.showing():
            continue
        interesting = (c.role() not in backend.boring_roles) or bool(c.name()) or c.actions()
        if interesting:
            lines.append("  " * depth + describe(c))
            walk(c, depth + 1, max_depth, lines, budget, only_showing)
        else:
            walk(c, depth, max_depth, lines, budget, only_showing)


# ---------------------------------------------------------------- tools


@mcp.tool()
def apps() -> str:
    """List applications with their top-level windows.
    Marks which are allowed and which window is active."""
    backend.enable()
    allow = allowed()
    lines = []
    for app in backend.list_apps():
        if not app.windows:
            continue
        flag = "allowed" if app.name.lower() in allow else "blocked"
        lines.append(f"{app.name} ({flag})")
        for w in app.windows:
            act = " [active]" if w.active else ""
            lines.append(f"  - {w.kind}: {w.title!r}{act}")
    lines.append(f"allowlist file: {ALLOW_FILE}")
    if backend.apps_note:
        lines.append(backend.apps_note)
    return "\n".join(lines)


@mcp.tool()
def tree(app: str = "", root_id: int = 0, max_depth: int = 12,
         max_nodes: int = 300, include_hidden: bool = False) -> str:
    """Return the accessibility tree as `[id] role "name" {states} actions=…`.
    Pass `app` (name from `apps`) for its windows, or `root_id` to expand a subtree.
    Ids are used by act/set_text/get_text/focus/click."""
    backend.enable()
    if root_id:
        root = resolve(root_id)
        lines = [describe(root)]
    else:
        root = find_app(app)
        lines = [app]
    walk(root, 1, max_depth, lines, max_nodes, not include_hidden)
    return "\n".join(lines)


@mcp.tool()
def find(query: str, app: str, role: str = "", max_results: int = 30) -> str:
    """Search an allowed app for elements whose name, text or role contains `query`
    (case-insensitive). Optionally filter by exact role name as shown by tree."""
    backend.enable()
    root = find_app(app)
    q = query.lower()
    hits: list[str] = []
    stack = [root]
    seen = 0
    while stack and len(hits) < max_results and seen < 20000:
        node = stack.pop()
        seen += 1
        for c in reversed(node.children()):
            if not c.showing():
                continue
            stack.append(c)
            r = c.role()
            if role and r != role:
                continue
            hay = f"{c.name()} {c.text(200)} {r}".lower()
            if q in hay:
                hits.append(describe(c))
    return "\n".join(hits) or "no matches"


@mcp.tool()
def act(id: int, action: str = "") -> str:
    """Perform an accessibility action on an element (default: its first action,
    usually click/press/invoke). Use the action names listed by tree/find."""
    node = resolve(id)
    names = node.actions()
    if not names:
        raise ValueError("element has no actions; try click, or focus + key")
    idx = 0
    if action:
        lowered = [n.lower() for n in names]
        if action.lower() not in lowered:
            raise ValueError(f"action '{action}' not in {names}")
        idx = lowered.index(action.lower())
    ok = node.do_action(idx)
    return f"{names[idx]} -> {'ok' if ok else 'failed'}"


@mcp.tool()
def set_text(id: int, text: str) -> str:
    """Replace the contents of an editable text element."""
    return "ok" if resolve(id).set_text(text) else "failed"


@mcp.tool()
def get_text(id: int, limit: int = 20000) -> str:
    """Read the full text of an element (documents, fields, labels)."""
    return resolve(id).full_text(limit)


@mcp.tool()
def focus(id: int) -> str:
    """Move keyboard focus to an element. Tries the accessibility focus request
    first, then a real click on the element's centre (the element's window must
    be active). Reports which method worked."""
    node = resolve(id)
    method = backend.try_focus(node)
    if method:
        return f"ok ({method})"
    require_active_allowed()
    x, y = backend.element_center(node)
    backend.pointer(x, y)
    time.sleep(0.15)
    if node.is_focused():
        return f"ok (clicked at {x},{y})"
    return f"clicked at {x},{y} but element did not report focus"


@mcp.tool()
def click(id: int, button: str = "left", clicks: int = 1) -> str:
    """Real pointer click on an element's centre, for elements without actions.
    The element's window must be active."""
    node = resolve(id)
    require_active_allowed()
    x, y = backend.element_center(node)
    backend.pointer(x, y, button, clicks)
    return f"clicked {button} x{clicks} at {x},{y}"


@mcp.tool()
def key(combo: str, repeat: int = 1) -> str:
    """Press a key or chord in the active window, e.g. "ctrl+s", "alt+tab", "enter",
    "f5", "left". Only sent when the active window belongs to an allowed app."""
    app = require_active_allowed()
    for _ in range(max(1, min(repeat, 100))):
        backend.press_combo(combo)
    return f"sent {combo} x{repeat} to {app}"


@mcp.tool()
def type_text(text: str) -> str:
    """Type text into the focused element of the active window. Works with any
    keyboard layout and Unicode. On Linux this goes through the clipboard and
    overwrites it."""
    app = require_active_allowed()
    backend.type_text(text)
    return f"typed {len(text)} chars into {app}"


@mcp.tool()
def mouse(x: int, y: int, button: str = "left", clicks: int = 1,
          scroll: int = 0) -> str:
    """Fallback pointer input in screen coordinates: move, then click
    (`clicks`=0 to only move) and/or scroll (positive = down)."""
    app = require_active_allowed()
    backend.pointer(x, y, button, clicks, scroll)
    return f"pointer at ({x},{y}) in {app}"


@mcp.tool()
def activate_window(query: str) -> str:
    """Bring an allowed app's window to the front by title or app-name substring."""
    backend.activate_window(query, allowed())
    return f"active app now: {backend.active_app() or 'unknown'}"


@mcp.tool()
def screenshot() -> str:
    """Last resort only: capture the active window to a PNG and return its path
    (read it with the Read tool). Prefer tree/find."""
    require_active_allowed()
    return backend.screenshot() or "screenshot failed"


def run_stdio() -> None:
    """Serve MCP on the real stdout while sending stray prints to stderr.

    Some platform libraries (uiautomation) print to stdout, which would corrupt
    the JSON-RPC stream, so the protocol keeps its own handle on fd 1.
    """
    import sys
    from io import TextIOWrapper

    import anyio
    from mcp.server.stdio import stdio_server

    proto_out = anyio.wrap_file(TextIOWrapper(sys.stdout.buffer, encoding="utf-8"))
    sys.stdout = sys.stderr

    async def main() -> None:
        async with stdio_server(stdout=proto_out) as (read_stream, write_stream):
            server = mcp._mcp_server
            await server.run(read_stream, write_stream,
                             server.create_initialization_options())

    anyio.run(main)


if __name__ == "__main__":
    import threading

    backend.enable()
    allowed()
    # Creating virtual input devices takes a moment; do it while the client
    # is still connecting so the first key/click is not delayed.
    threading.Thread(target=backend.warm_up, daemon=True).start()
    run_stdio()
