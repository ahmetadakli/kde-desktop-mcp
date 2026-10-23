"""Latency benchmark of MCP servers over stdio: same calls, median ms per tool.

    .venv/bin/python tools/mcp_bench.py --app kate --window notes.txt \\
        --server "Python=.venv/bin/python server.py" \\
        --server "Rust=rust/target/release/desktop-mcp"

The window must already be open. Each server is started fresh per round and
runs the same sequence: initialize, apps, activate_window, tree, find,
get_text, key, type_text, focus, click, mouse, screenshot. The editor element
is the visible editable element named after --window (else the first one
found). Typed characters are removed again with backspace, so the
document is left as it was. Prints a Markdown table of medians.

It sends keys and clicks to the window; do not use the keyboard or mouse
while it runs.

For partial platforms (e.g. Windows under Wine, whose UI Automation client
lacks most of the API) pass --tolerant: failing steps are counted as failures
instead of aborting the round, steps that need an element id are skipped when
none was found, and `mouse` goes to --mouse-at. --pre runs a command before
every round (e.g. to bring the window to the front when activate_window
cannot).
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import statistics
import subprocess
import sys
import time

STEPS = ["initialize", "apps", "activate_window", "tree", "find", "get_text", "key",
         "type_text", "focus", "click", "mouse", "screenshot"]


class Client:
    def __init__(self, cmd: str):
        self.proc = subprocess.Popen(shlex.split(cmd), stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                     text=True, encoding="utf-8")
        self.n = 0

    def rpc(self, method: str, params: dict | None = None, notify: bool = False) -> dict:
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self.n += 1
            msg["id"] = self.n
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        if notify:
            return {}
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server exited")
            reply = json.loads(line)
            if reply.get("id") == self.n:
                return reply

    def call(self, name: str, **args) -> tuple[float, str, bool]:
        t = time.perf_counter()
        res = self.rpc("tools/call", {"name": name, "arguments": args})["result"]
        ms = 1000 * (time.perf_counter() - t)
        text = "".join(c.get("text", "") for c in res.get("content", []))
        return ms, text, not res.get("isError", False)

    def close(self) -> None:
        self.proc.terminate()
        self.proc.wait()


def one_round(cmd: str, app: str, window: str, query: str, roles: list[str],
              tolerant: bool = False, mouse_at: tuple[int, int] | None = None,
              failures: dict[str, str] | None = None) -> dict[str, float]:
    times: dict[str, float] = {}
    failures = {} if failures is None else failures
    c = Client(cmd)
    try:
        t = time.perf_counter()
        c.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                             "clientInfo": {"name": "bench", "version": "0"}})
        c.rpc("notifications/initialized", notify=True)
        times["initialize"] = 1000 * (time.perf_counter() - t)
        time.sleep(0.5)  # let a background warm-up finish, as in real use

        def step(name: str, **args) -> str:
            ms, text, ok = c.call(name, **args)
            if not ok:
                if tolerant:
                    failures.setdefault(name, text)
                    return ""
                raise RuntimeError(f"{name}: {text}")
            times[name] = ms
            return text

        step("apps")
        step("activate_window", query=window)
        step("tree", app=app)
        found = ""
        for role in roles:
            found = step("find", query=query, app=app, role=role)
            if found != "no matches":
                break
        # Prefer the editor named after the window's document (another
        # window of the same app may be open).
        editable = [line for line in found.splitlines() if "editable" in line]
        editable.sort(key=lambda line: window not in line)
        rid = int(re.match(r"\s*\[(\d+)\]", editable[0]).group(1)) if editable else None
        if rid is None and not tolerant:
            raise RuntimeError(f"no editable element found:\n{found}")
        if rid is not None:
            step("get_text", id=rid)
        step("key", combo="end")
        if step("type_text", text="x"):
            c.call("key", combo="backspace")
        center = None
        if rid is not None:
            step("focus", id=rid)
            center = re.search(r"at (\d+),(\d+)", step("click", id=rid))
        if center:
            step("mouse", x=int(center.group(1)), y=int(center.group(2)), clicks=0)
        elif mouse_at:
            step("mouse", x=mouse_at[0], y=mouse_at[1], clicks=0)
        step("screenshot")
    finally:
        c.close()
    return times


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--server", action="append", required=True, help="Label=command")
    ap.add_argument("--app", required=True)
    ap.add_argument("--window", required=True, help="substring of the window title")
    ap.add_argument("--query", default="", help="find query (default: all elements)")
    ap.add_argument("--role", action="append",
                    help="editor role(s) to search, first match wins (default: text)")
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--tolerant", action="store_true",
                    help="count failing steps instead of aborting the round")
    ap.add_argument("--mouse-at", help="X,Y for mouse when no element centre is known")
    ap.add_argument("--pre", help="shell command run before every round")
    args = ap.parse_args()
    roles = args.role or ["text"]
    mouse_at = tuple(int(v) for v in args.mouse_at.split(",")) if args.mouse_at else None

    servers = [s.split("=", 1) for s in args.server]
    results: dict[str, dict[str, list[float]]] = {label: {} for label, _ in servers}
    failed: dict[str, dict[str, str]] = {label: {} for label, _ in servers}
    for r in range(args.rounds):
        for label, cmd in servers:  # alternate, so drift hits both alike
            if args.pre:
                subprocess.run(args.pre, shell=True, check=False)
            for k, v in one_round(cmd, args.app, args.window, args.query, roles,
                                  args.tolerant, mouse_at, failed[label]).items():
                results[label].setdefault(k, []).append(v)
        print(f"round {r + 1}/{args.rounds} done", flush=True)

    labels = [label for label, _ in servers]
    print("\n| İşlem | " + " | ".join(f"{label} (ms)" for label in labels)
          + (" | Hızlanma |" if len(labels) == 2 else " |"))
    print("|---|" + "---:|" * len(labels) + ("---:|" if len(labels) == 2 else ""))
    for name in STEPS:
        meds = [statistics.median(results[label][name]) if name in results[label] else None
                for label in labels]
        cells = [f"{m:.1f}" if m is not None else ("fail" if name in failed[label] else "—")
                 for m, label in zip(meds, labels)]
        row = f"| `{name}` | " + " | ".join(cells) + " |"
        if len(labels) == 2 and all(meds) and meds[1] > 0:
            row += f" {meds[0] / meds[1]:.1f}x |"
        print(row)
    print(f"\nmedian of {args.rounds} rounds; raw: {json.dumps(results)}")
    for label in labels:
        for name, text in failed[label].items():
            print(f"{label} {name} failed: {text}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
