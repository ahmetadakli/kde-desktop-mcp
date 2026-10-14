"""End-to-end smoke test on the real desktop.

Opens a temporary file in the platform editor (Kate on KDE and Hyprland, Notepad on Windows),
then drives it only through the MCP tool functions: list, find, focus after
focus moved away, type Unicode text, save with a shortcut, read back, and check
that the allowlist blocks an unknown app.

Run from the repository root:
    .venv/bin/python tools/smoke_test.py          (Linux)
    .venv\\Scripts\\python tools\\smoke_test.py    (Windows)

It moves focus and types into the editor window it opens; do not use the
keyboard or mouse while it runs.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import server  # noqa: E402

if sys.platform == "win32":
    APP, LAUNCH, EDITOR_ROLES = "notepad", ["notepad.exe"], ("document", "edit")
else:
    APP, LAUNCH, EDITOR_ROLES = "kate", ["kate", "-n"], ("text",)

TYPED = "Smoke test: çğışöü İ"
results: list[tuple[str, bool, str]] = []


def step(name: str, ok: bool, detail: str = "") -> bool:
    results.append((name, ok, detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {name}" + (f": {detail}" if detail else ""))
    return ok


def wait_for(fn, timeout: float = 15.0):
    deadline = time.monotonic() + timeout
    last_exc = None
    while time.monotonic() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception as exc:  # retried until the deadline
            last_exc = exc
        time.sleep(0.5)
    if last_exc:
        raise last_exc
    return None


def editor_node(stem: str):
    """First visible editable node with an editor role in the app's window."""
    root = server.find_app(APP)
    stack = list(reversed(root.children()))
    while stack:
        node = stack.pop()
        if not node.showing():
            continue
        if node.role() in ("frame", "window") and stem not in node.name():
            continue  # another window of the same app
        if node.role() in EDITOR_ROLES and "editable" in node.states():
            return node
        stack.extend(reversed(node.children()))
    return None


def main() -> int:
    if APP not in server.allowed():
        print(f"'{APP}' is not in {server.ALLOW_FILE}; add it and rerun.")
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="desktop-mcp-smoke-"))
    path = tmp / f"{tmp.name}.txt"  # unique, so leftover windows never match
    path.write_text("ilk satır\n", encoding="utf-8")
    stem = path.stem  # Notepad may hide the extension in the title

    server.backend.enable()
    subprocess.Popen(LAUNCH + [str(path)], stdout=subprocess.DEVNULL,
                     stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)

    listed = wait_for(lambda: stem in server.apps())
    step("apps lists the editor window", bool(listed))

    server.activate_window(stem)
    active = wait_for(lambda: server.backend.active_app().lower() == APP)
    step("activate_window", bool(active), server.backend.active_app())

    node = wait_for(lambda: editor_node(stem))
    if not step("find editor element", node is not None):
        return 1
    rid = server.ref(node)
    step("get_text reads file", "ilk satır" in server.get_text(rid),
         repr(server.get_text(rid)[:40]))

    server.key("ctrl+f")
    time.sleep(0.7)
    step("focus moved away (ctrl+f)", not node.is_focused())

    msg = server.focus(rid)
    step("focus returns to editor", msg.startswith("ok"), msg)

    server.key("ctrl+end")
    server.type_text("\n" + TYPED)
    time.sleep(0.3)
    step("typed text visible in editor", TYPED in server.get_text(rid))

    server.key("ctrl+s")
    saved = wait_for(lambda: TYPED in path.read_text(encoding="utf-8"), timeout=5)
    step("ctrl+s saved Unicode text to disk", bool(saved),
         repr(path.read_text(encoding="utf-8")))

    try:
        server.find_app("definitely-not-allowed-app")
        step("allowlist blocks unknown app", False)
    except PermissionError:
        step("allowlist blocks unknown app", True)

    failed = [n for n, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} passed. Test file: {path}")
    print("Close the editor window when done.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
