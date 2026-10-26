"""End-to-end smoke test of an MCP server binary over stdio (Linux, Kate).

Speaks the real MCP protocol, so it checks any server command; needs only
the Python standard library:

    python3 tools/mcp_smoke_test.py rust/target/release/desktop-mcp

Opens a temporary file in Kate and drives it only through tool calls: list,
find, focus after focus moved away, type Unicode text, rapid typing, key
repeat, batch, click, save with a shortcut, read back from disk, and the
allowlist. Prints the latency of every call.

It moves focus and types into the editor window it opens; do not use the
keyboard or mouse while it runs.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TYPED = "Smoke test: çğışöü İ"
RAPID = 10
results: list[tuple[str, bool, str]] = []
timings: dict[str, list[float]] = {}


class Client:
    def __init__(self, cmd: list[str]):
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
        self.n = 0
        t = time.perf_counter()
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "smoke", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        timings.setdefault("initialize", []).append(time.perf_counter() - t)

    def send(self, msg: dict) -> None:
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def rpc(self, method: str, params: dict) -> dict:
        self.n += 1
        self.send({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server exited")
            msg = json.loads(line)
            if msg.get("id") == self.n:
                return msg

    def call(self, name: str, **args) -> tuple[bool, str]:
        """(ok, text) of a tool call."""
        t = time.perf_counter()
        res = self.rpc("tools/call", {"name": name, "arguments": args})["result"]
        timings.setdefault(name, []).append(time.perf_counter() - t)
        text = "".join(c.get("text", "") for c in res.get("content", []))
        return not res.get("isError", False), text

    def text(self, name: str, **args) -> str:
        ok, text = self.call(name, **args)
        if not ok:
            raise RuntimeError(text)
        return text

    def close(self) -> None:
        self.proc.terminate()


def step(name: str, ok: bool, detail: str = "") -> bool:
    results.append((name, ok, detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {name}" + (f": {detail}" if detail else ""))
    return ok


def wait_for(fn, timeout: float = 15.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = fn()
            if value:
                return value
        except RuntimeError:
            pass
        time.sleep(0.3)
    return None


def editor_id(c: Client) -> int | None:
    """Id of Kate's editable text view showing the test file."""
    for line in c.text("find", query="", app="kate", role="text").splitlines():
        if "editable" in line and "ilk satır" in line:
            return int(re.match(r"\s*\[(\d+)\]", line).group(1))
    return None


def is_focused(c: Client, rid: int) -> bool:
    header = c.text("tree", root_id=rid, max_depth=0).splitlines()[0]
    return "focused" in header


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    c = Client(sys.argv[1:])

    tmp = Path(tempfile.mkdtemp(prefix="desktop-mcp-smoke-"))
    path = tmp / f"{tmp.name}.txt"  # unique, so leftover windows never match
    path.write_text("ilk satır\n", encoding="utf-8")
    stem = path.stem
    kate = subprocess.Popen(["kate", "-n", str(path)], stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
    try:
        step("apps lists the editor window", bool(wait_for(lambda: stem in c.text("apps"))))

        msg = wait_for(lambda: (m := c.text("activate_window", query=stem)).endswith("kate") and m)
        step("activate_window", bool(msg), msg or "")

        rid = wait_for(lambda: editor_id(c))
        if not step("find editor element", rid is not None):
            return 1
        step("get_text reads file", "ilk satır" in c.text("get_text", id=rid))

        c.text("key", combo="ctrl+f")
        time.sleep(0.7)
        step("focus moved away (ctrl+f)", not is_focused(c, rid))
        msg = c.text("focus", id=rid)
        step("focus returns to editor", msg.startswith("ok"), msg)

        ok, out = c.call("batch", steps=[
            {"tool": "key", "args": {"combo": "ctrl+end"}},
            {"tool": "type_text", "args": {"text": "\n" + TYPED}},
        ])
        step("batch runs key + type_text", ok and out.count("\n") == 1, out.replace("\n", " | "))
        time.sleep(0.2)
        step("typed text visible in editor", TYPED in c.text("get_text", id=rid))

        for i in range(RAPID):
            c.text("type_text", text=f"\nhızlı-{i}")
        time.sleep(0.2)
        body = c.text("get_text", id=rid)
        lines = [f"hızlı-{i}" for i in range(RAPID)]
        step(f"{RAPID} rapid type_text calls arrive in order",
             "\n".join(lines) in body, repr(body[-80:]))

        c.text("type_text", text="XYZ")
        c.text("key", combo="backspace", repeat=3)
        time.sleep(0.2)
        body = c.text("get_text", id=rid)
        step("key repeat (3x backspace)", body.rstrip().endswith(f"hızlı-{RAPID - 1}"),
             repr(body[-30:]))

        msg = c.text("click", id=rid)
        step("click on element", msg.startswith("clicked left x1 at"), msg)

        c.text("key", combo="ctrl+s")
        saved = wait_for(lambda: TYPED in path.read_text(encoding="utf-8")
                         and f"hızlı-{RAPID - 1}" in path.read_text(encoding="utf-8"), timeout=5)
        step("ctrl+s saved Unicode text to disk", bool(saved),
             repr(path.read_text(encoding="utf-8")[-60:]))

        second = tmp / f"{tmp.name}-launch.txt"
        second.write_text("ikinci\n", encoding="utf-8")
        ok, out = c.call("launch", app="kate", args=[str(second)])
        step("launch opens a file in kate and returns its window",
             ok and "[" in out and " at (" in out, out.replace("\n", " | "))
        ok, out = c.call("launch", app="definitely-not-allowed-app")
        step("launch refuses an app not in the allowlist", not ok and "not in" in out, out)

        ok, out = c.call("find", query="x", app="definitely-not-allowed-app")
        step("allowlist blocks unknown app", not ok and "not in" in out, out)
        ok, out = c.call("get_text", id=999999999)
        step("unknown id is an error", not ok and "unknown id" in out, out)
    finally:
        c.close()
        kate.terminate()

    print("\nlatency (ms): " + ", ".join(
        f"{k} {1000 * min(v):.0f}" + (f"-{1000 * max(v):.0f}" if len(v) > 1 else "")
        for k, v in timings.items()))
    failed = [n for n, ok, _ in results if not ok]
    print(f"{len(results) - len(failed)}/{len(results)} passed. Test file: {path}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
