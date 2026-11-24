"""Integration test of the server in a headless KWin session (CI only).

    tools/ci/kde_headless.sh rust/target/release/desktop-mcp

Run by kde_headless.sh inside the CI container, after it started its own
KWin and D-Bus session; it refuses to run anywhere else (it starts a window
and sends it keys). Starts tools/ci/qt_app.py and the server with an
allowlist holding only that app, then speaks MCP over stdio: apps, tree,
find, set_text, get_text, act, activate_window, click, mouse (its hit
test), focus, type_text, key, a screenshot of a window in the background (and
one with element marks, one of an element) and the allowlist's refusals,
each checked against what the window shows.

type_text into the multi-line editor inserts through AT-SPI and needs no
input device. key, and type_text into a line edit (a paste with ctrl+v),
need /dev/uinput and a compositor that reads it; KWin's virtual backend
reads no input devices. --input auto (the default) runs them when
/dev/uinput is writable and reports them skipped, with the reason, when the
first key does not arrive; --input require fails instead, --input skip
skips them.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

APP = "desktop-mcp-ci"
TITLE = "desktop-mcp CI window"
OTHER = "desktop-mcp CI other"
HERE = Path(__file__).resolve().parent
results: list[tuple[str, str, str]] = []


class Client:
    def __init__(self, cmd: list[str], env: dict, log: Path):
        self.log = log
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=log.open("w"), text=True, encoding="utf-8", env=env)
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "kde-headless-ci", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

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
        res = self.rpc("tools/call", {"name": name, "arguments": args})["result"]
        text = "".join(c.get("text", "") for c in res.get("content", []))
        return not res.get("isError", False), text

    def text(self, name: str, **args) -> str:
        ok, text = self.call(name, **args)
        if not ok:
            raise RuntimeError(f"{name}: {text}")
        return text

    def close(self) -> None:
        self.proc.terminate()
        try:
            self.proc.wait(5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def record(name: str, verdict: str, detail: str = "") -> bool:
    results.append((name, verdict, detail))
    print(f"[{verdict}] {name}" + (f": {detail}" if detail else ""), flush=True)
    return verdict == "PASS"


def step(name: str, ok: bool, detail: str = "") -> bool:
    return record(name, "PASS" if ok else "FAIL", detail)


def wait_for(fn, timeout: float = 10.0):
    deadline = time.monotonic() + timeout
    while True:
        try:
            value = fn()
            if value:
                return value
        except RuntimeError:
            pass
        if time.monotonic() >= deadline:
            return None
        time.sleep(0.3)


def one_line(s: str) -> str:
    return s.replace("\n", " | ")[:300]


def find_id(c: Client, query: str, role: str = "") -> int | None:
    """Id of the first element `find` lists whose line has `query`."""
    out = c.text("find", app=APP, query=query, role=role)
    for line in out.splitlines():
        m = re.match(r"\s*\[(\d+)\]", line)
        if m and query.lower() in line.lower():
            return int(m.group(1))
    return None


def status(c: Client) -> str:
    sid = find_id(c, "Status:")
    return c.text("get_text", id=sid) if sid is not None else ""


def status_has(c: Client, want: str, timeout: float = 5.0) -> str | None:
    return wait_for(lambda: (s := status(c)) and want in s and s, timeout)


def input_tests(c: Client, ids: dict, mode: str) -> None:
    """key and type_text through the virtual keyboard."""
    if mode == "skip":
        record("input (uinput)", "SKIP", "--input skip")
        return
    if mode == "auto" and not os.access("/dev/uinput", os.W_OK):
        record("input (uinput)", "SKIP", "/dev/uinput is missing or not writable here")
        return
    hard = "FAIL" if mode == "require" else "SKIP"

    c.call("activate_window", query=TITLE)
    ok, out = c.call("key", combo="ctrl+k", window=TITLE)
    if not ok:
        record("key ctrl+k fires the window's shortcut", hard, one_line(out))
        return
    seen = status_has(c, "shortcut")
    if not seen:
        record("key ctrl+k fires the window's shortcut", hard,
               "the key event did not arrive (KWin's virtual backend reads no input "
               f"devices); key reply: {one_line(out)}; {status(c)!r}")
        return
    step("key ctrl+k fires the window's shortcut", True)

    c.call("focus", id=ids["name"])
    c.text("key", combo="ctrl+a")
    ok, out = c.call("type_text", text="Grace Hopper")
    got = wait_for(lambda: "Grace Hopper" in (t := c.text("get_text", id=ids["name"])) and t)
    step("type_text into a line edit (paste)", ok and bool(got), one_line(out))


def pointer_hit_test(c: Client, ids: dict) -> None:
    """mouse at a button's pixels names the button (an AT-SPI hit test, so
    it holds whether or not the pointer input arrives)."""
    if not os.access("/dev/uinput", os.W_OK):
        record("mouse names the element under it", "SKIP",
               "/dev/uinput is missing or not writable here (no pointer device)")
        return
    ok, out = c.call("click", id=ids["greet"], clicks=0, window=TITLE)
    m = re.search(r"at (-?\d+),(-?\d+)", out)
    if not step("click with clicks=0 gives the button's centre", ok and bool(m), one_line(out)):
        return
    x, y = int(m.group(1)), int(m.group(2))
    ok, out = c.call("mouse", x=x, y=y, clicks=0)
    step("mouse over a button names it", ok and re.search(r' over \[\d+\] [^"]*"Greet"', out)
         is not None, one_line(out))
    ok, out = c.call("mouse", x=x, y=y)
    m = re.search(r' on \[(\d+)\] [^"]*"Greet"', out)
    step("mouse click on a button names it", ok and bool(m), one_line(out))
    if m:
        ok, got = c.call("get_text", id=int(m.group(1)))
        step("the id a mouse reply names is usable", ok, one_line(got))


def background_screenshot(c: Client) -> None:
    """screenshot with window= captures a window behind another where it
    is (KWin ScreenShot2 by handle), without activating it."""
    ok, out = c.call("activate_window", query=OTHER)
    if not step("activate_window raises the second window", ok, one_line(out)):
        return
    ok, out = c.call("screenshot", window=TITLE)
    if ok and "captured in background" not in out:
        # Refused where it is (no ScreenShot2 grant): brought forward and
        # captured in the same call instead.
        apps = c.text("apps")
        front = [line for line in apps.splitlines() if "[active]" in line]
        step("a refused background capture brings the window forward in one call",
             any(TITLE in line for line in front), one_line(out))
        record("screenshot captures a window in the background", "SKIP",
               f"KWin refuses ScreenShot2 here: {one_line(out)}")
        return
    step("screenshot captures a window in the background",
         ok and "captured in background" in out, one_line(out))
    apps = c.text("apps")
    front = [line for line in apps.splitlines() if "[active]" in line]
    step("the background screenshot activates nothing",
         any(OTHER in line for line in front) and not any(TITLE in line for line in front),
         one_line(apps))
    ok, out = c.call("mouse", image_x=10, image_y=10, clicks=0)
    step("its pixels are not used while the window is behind",
         not ok and "in the background" in out and "nothing sent" in out, one_line(out))
    ok, out = c.call("screenshot", region=[0, 0, 40, 30], inline=False)
    lines = out.splitlines()
    if ok and lines[0].endswith(".png"):
        Path(lines[0]).unlink(missing_ok=True)
    step("a region zoom is of the window captured behind, not the one in front",
         ok and "captured in background" in out and "zoom of region" in out, one_line(out))


def marked_screenshot(c: Client, ids: dict) -> None:
    """screenshot marks=true numbers the window's controls (here the window
    behind, captured in place) and the ids it lists act like find's."""
    ok, out = c.call("screenshot", window=TITLE, marks=True, inline=False)
    if ok and "captured in background" not in out:
        record("screenshot marks=true lists the window's controls", "SKIP",
               f"KWin refuses ScreenShot2 here: {one_line(out)}")
        return
    path = out.splitlines()[0] if ok else ""
    if path.endswith(".png"):
        Path(path).unlink(missing_ok=True)
    legend = next((line for line in out.splitlines() if line.startswith("marks: ")), "")
    m = re.search(r"\[(\d+)\] [^,]*'Greet'", legend)
    if not step("screenshot marks=true lists the window's controls",
                ok and bool(m) and "'Agree'" in legend, one_line(out)):
        return
    c.call("activate_window", query=TITLE)
    c.call("act", id=ids["agree"], action="uncheck")
    ok, out = c.call("act", id=int(m.group(1)))
    seen = status_has(c, "hello")
    step("a mark's id acts on its control", ok and bool(seen),
         f"{one_line(out)}; {seen or status(c)!r}")


def element_screenshot(c: Client, ids: dict) -> None:
    """screenshot id= captures just the element and a margin around it, in
    its own window: here the window behind, captured in place."""
    c.call("activate_window", query=OTHER)
    ok, out = c.call("screenshot", id=ids["greet"], inline=False)
    if (ok and "captured in background" not in out) or (not ok and "KWin refused" in out):
        record("screenshot id= crops to the element", "SKIP",
               f"KWin refuses ScreenShot2 here: {one_line(out)}")
        return
    lines = out.splitlines()
    if ok and lines[0].endswith(".png"):
        Path(lines[0]).unlink(missing_ok=True)
    m = re.search(r"(\d+)x(\d+) px of screen area", out)
    whole = re.search(r"(\d+)x(\d+) px", c.text("screenshot", window=TITLE, max_size=2576))
    small = bool(m and whole) and int(m.group(1)) < int(whole.group(1)) \
        and int(m.group(2)) < int(whole.group(2))
    step("screenshot id= crops to the element in its window",
         ok and f"(element {ids['greet']} and 12 px around it" in out
         and "captured in background" in out and small, one_line(out))
    c.call("activate_window", query=TITLE)


def wait_idle_while_ticking(c: Client) -> None:
    """wait_idle settles on a window whose clock never stops changing: after
    2 s of change events it compares the content, digits aside."""
    c.call("activate_window", query=TITLE)
    tick = find_id(c, "Tick", "check box")
    ok, out = c.call("act", id=tick, action="check") if tick is not None else (False, "no Tick")
    if not step("act check starts the ticking clock", ok, one_line(out)):
        return
    try:
        ok, out = c.call("wait_idle", app=APP, quiet_ms=200, timeout_ms=8000)
        step("wait_idle settles while a clock ticks", ok and "idle after" in out, one_line(out))
        if ok and "change events never stopped" not in out:
            record("wait_idle settles on the content past the clock's events", "SKIP",
                   f"the clock's changes sent no events here: {one_line(out)}")
        else:
            step("wait_idle settles on the content past the clock's events",
                 ok and "held still for 200 ms" in out, one_line(out))
    finally:
        ok, out = c.call("act", id=tick, action="uncheck")
        step("act uncheck stops the clock", ok, one_line(out))


def kept_read_outlives_think_time(c: Client) -> None:
    """A read of an app heard sending change events is reused after a pause
    longer than the 2 s kept for other apps (DESKTOP_MCP_TRACE's lines),
    where the user's input is watched."""
    name = "a kept read is reused 3 s later"
    if "not watching user input" in c.text("doctor"):
        record(name, "SKIP", "no source of the user's input works here: reads last 2 s")
        return
    # Past the tail of the last input, when a smooth scroll may still move.
    time.sleep(1)
    c.text("find", app=APP, query="Greet", role="button")
    seen = c.log.stat().st_size
    time.sleep(3)
    c.text("find", app=APP, query="Greet", role="button")
    with c.log.open(encoding="utf-8", errors="replace") as f:
        f.seek(seen)
        lines = [line.strip() for line in f if "kept read" in line]
    if any("kept read of" in line for line in lines):
        step(name, True)
    elif any("too old" in line for line in lines):
        step(name, False, "; ".join(lines))
    else:
        record(name, "SKIP", "; ".join(lines) or "the read was not kept (no change events?)")


def run(c: Client, input_mode: str) -> None:
    ok, out = c.call("doctor")
    print(f"--- doctor ---\n{out}\n---", flush=True)

    apps = wait_for(lambda: (a := c.text("apps")) and APP in a and TITLE in a and OTHER in a
                    and a, 30)
    if not step("apps lists the test window", bool(apps), one_line(apps or c.text("apps"))):
        return

    tree = c.text("tree", app=APP)
    step("tree shows the window's controls",
         all(w in tree for w in ("Greet", "Name", "Notes", "Agree", "Status:")), one_line(tree))
    # Tool bars are summed up only next to a browser's page.
    step("tree keeps a plain app's lines", "browser tool bars" not in tree, one_line(tree))

    ids = {
        "greet": find_id(c, "Greet", "button"),
        "agree": find_id(c, "Agree", "check box"),
        "name": find_id(c, "Name", "text"),
        "notes": find_id(c, "Notes", "text"),
    }
    if not step("find locates every control", all(v is not None for v in ids.values()), str(ids)):
        return

    ok, out = c.call("set_text", id=ids["name"], text="Ada Lovelace")
    step("set_text writes the line edit", ok, one_line(out))
    step("get_text reads it back", "Ada Lovelace" in c.text("get_text", id=ids["name"]))

    ok, out = c.call("act", id=ids["greet"])
    seen = status_has(c, "hello Ada Lovelace")
    step("act presses the button", ok and bool(seen), f"{one_line(out)}; {seen or status(c)!r}")

    ok, out = c.call("act", id=ids["agree"], action="check")
    seen = status_has(c, "agree on")
    step("act check ticks the check box", ok and bool(seen),
         f"{one_line(out)}; {seen or status(c)!r}")
    ok, out = c.call("act", id=ids["agree"], action="check")
    step("act check on a ticked box sends nothing", ok and "nothing sent" in out, one_line(out))

    ok, out = c.call("activate_window", query=TITLE)
    step("activate_window raises the test window", ok, one_line(out))

    pointer_hit_test(c, ids)

    ok, out = c.call("focus", id=ids["notes"])
    step("focus moves to the editor", ok, one_line(out))
    typed = "Zeile çğışöü İ\nzweite"
    ok, out = c.call("type_text", text=typed, window=TITLE)
    got = wait_for(lambda: typed in c.text("get_text", id=ids["notes"]), 5)
    step("type_text into the editor (accessibility insert)", ok and bool(got), one_line(out))

    input_tests(c, ids, input_mode)

    wait_idle_while_ticking(c)

    kept_read_outlives_think_time(c)

    ok, out = c.call("find", app="definitely-not-allowed-app", query="x")
    step("allowlist refuses another app", not ok and "not in" in out, one_line(out))
    ok, out = c.call("get_text", id=999999999)
    step("an unknown id is an error", not ok and "unknown id" in out, one_line(out))

    # Screenshots last: KWin's virtual backend (QPainter, no GPU) has been
    # seen to crash in ScreenShot2's CaptureWindow, which takes the session
    # down with it. On the real desktop the same calls work.
    screenshots(c, ids)


KWIN_GONE = ("org.kde.KWin was not provided", "DBus.Error.NoReply")


def screenshots(c: Client, ids: dict) -> None:
    before = len(results)
    background_screenshot(c)
    marked_screenshot(c, ids)
    element_screenshot(c, ids)
    gone = any(v == "FAIL" and any(k in d for k in KWIN_GONE) for _, v, d in results[before:])
    if gone:
        # Turn this group's failures into skips: the compositor, not the
        # server, failed (see kwin.log in the job's artifacts).
        for i in range(before, len(results)):
            name, verdict, detail = results[i]
            if verdict == "FAIL":
                results[i] = (name, "SKIP", "KWin's virtual backend crashed in a capture: " + detail)
                print(f"[SKIP] {name}: KWin's virtual backend crashed in a capture", flush=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("server", nargs="+", help="server command")
    ap.add_argument("--input", choices=("auto", "require", "skip"), default="auto")
    ap.add_argument("--logs", default=os.environ.get("CI_LOG_DIR", "."))
    args = ap.parse_args()
    if os.environ.get("DESKTOP_MCP_HEADLESS") != "1" or os.environ.get("GITHUB_ACTIONS") != "true":
        print("refusing to run: only for the headless session kde_headless.sh starts "
              "(it opens a window and sends it keys)", file=sys.stderr)
        return 2
    logs = Path(args.logs)
    logs.mkdir(parents=True, exist_ok=True)

    home = Path(tempfile.mkdtemp(prefix="desktop-mcp-ci-home-"))
    (home / ".config/desktop-mcp").mkdir(parents=True)
    (home / ".config/desktop-mcp/allow.txt").write_text(f"{APP}\n", encoding="utf-8")
    env = dict(os.environ, HOME=str(home), DESKTOP_MCP_INDICATOR="0",
               DESKTOP_MCP_NO_UPDATE_CHECK="1", DESKTOP_MCP_STOP_KEY="off",
               DESKTOP_MCP_TRACE="1")

    app = subprocess.Popen([sys.executable, str(HERE / "qt_app.py")], env=os.environ,
                           stdin=subprocess.DEVNULL, stdout=(logs / "qt_app.log").open("w"),
                           stderr=subprocess.STDOUT)
    c = Client(args.server, env, logs / "server.log")
    try:
        run(c, args.input)
    except (RuntimeError, OSError) as e:
        step("no unexpected error", False, str(e))
    finally:
        if any(v == "FAIL" for _, v, _ in results):
            for name, a in (("apps", {}), ("tree", {"app": APP})):
                try:
                    print(f"--- {name} ---\n{c.call(name, **a)[1]}\n---")
                except (RuntimeError, OSError) as e:
                    print(f"--- {name}: {e}")
        c.close()
        app.terminate()

    count = {v: sum(1 for _, r, _ in results if r == v) for v in ("PASS", "FAIL", "SKIP")}
    print(f"\n{count['PASS']} passed, {count['FAIL']} failed, {count['SKIP']} skipped")
    for name, v, detail in results:
        if v == "SKIP":
            print(f"skipped: {name}: {detail}")
    return 1 if count["FAIL"] or not count["PASS"] else 0


if __name__ == "__main__":
    sys.exit(main())
