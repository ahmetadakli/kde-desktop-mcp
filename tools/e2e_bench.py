"""End-to-end benchmark: how long real desktop tasks take an agent.

Runs Claude Code headless (`claude -p`) on desktop tasks with only this
server's tools and none of the user's settings, plugins, hooks or skills, and
measures, per task, model and server binary: model turns, desktop tool calls,
wall time (to the final result) split into startup, model and tool time, the
exit after it, the size of tool replies (characters, images), tokens, and
whether the task was done (a check on files or on a read-only desktop-mcp
query).

    python3 tools/e2e_bench.py --tasks kate_append \\
        --server "main=$HOME/kde-desktop-mcp/rust/target/release/desktop-mcp" \\
        --server "new=rust/target/release/desktop-mcp" --repeat 3

Tasks live in tools/e2e_tasks.json (see its "_doc"): each has files and
shell `setup` steps that open an app on a fresh temp dir, a `prompt` that
states the goal as a user would, a `check`, and cleanup (the exact PIDs its
setup recorded, or the one window whose title holds the run's unique tag).

Prints a Markdown table of medians and writes results.json, table.md and
every run's raw stream (.jsonl) to --results (default: a new temp dir; never
inside the repository). Linux (KDE Plasma) only; needs `claude` on PATH.

The agent sends real keyboard and mouse input to the windows the tasks open
(each run starts with its window made active); do not use the keyboard or
mouse while it runs. Firefox and Dolphin open task windows in a running
instance: close your own windows of them first, or pass --exclusive to skip
those runs. Ctrl+C stops the agent's whole process group before cleanup.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import shlex
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_TASKS = Path(__file__).resolve().parent / "e2e_tasks.json"
DEFAULT_SERVER = REPO / "rust" / "target" / "release" / "desktop-mcp"
DEFAULT_MODELS = ["default", "claude-sonnet-5-5", "claude-haiku-5-5"]
TOOL_PREFIX = "mcp__desktop__"
# Tools a check may call: they read and never send input.
READ_ONLY = {"apps", "windows", "tree", "find", "get_text", "wait_for", "wait_idle"}


class Mcp:
    """Minimal MCP stdio client for readiness waits, checks and cleanup."""

    def __init__(self, binary: str):
        self.proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "e2e-bench", "version": "0"}})
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
                raise RuntimeError("desktop-mcp exited")
            msg = json.loads(line)
            if msg.get("id") == self.n:
                return msg

    def call(self, name: str, **args) -> tuple[bool, str]:
        res = self.rpc("tools/call", {"name": name, "arguments": args}).get("result", {})
        text = "".join(c.get("text", "") for c in res.get("content", []))
        return not res.get("isError", False), text

    def close(self) -> None:
        self.proc.terminate()
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def fill(text: str, subs: dict[str, str]) -> str:
    for k, v in subs.items():
        text = text.replace("{" + k + "}", v)
    return text


def run_shell(cmds: list[str], env: dict[str, str], cwd: Path, what: str) -> None:
    for cmd in cmds:
        r = subprocess.run(["bash", "-c", cmd], env=env, cwd=cwd, stdin=subprocess.DEVNULL,
                           capture_output=True, text=True, timeout=60)
        if r.returncode != 0:
            raise RuntimeError(f"{what} step failed ({r.returncode}): {cmd}\n{r.stderr.strip()}")


def wait_window(mcp: Mcp, title: str, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ok, text = mcp.call("windows", query=title)
        if ok and title in text:
            return True
        time.sleep(0.3)
    return False


def pid_cmdline(pid: int) -> str:
    try:
        return Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode(
            "utf-8", "replace")
    except OSError:
        return ""


def naming_dir(task_dir: Path) -> list[int]:
    """PIDs of this user's processes whose command line names task_dir (a
    unique temp dir: only processes this run started), e.g. a
    `dolphin --new-window` that did not hand over to a running instance."""
    pids = []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit() and int(entry.name) != os.getpid():
            try:
                if entry.stat().st_uid == os.getuid() and str(task_dir) in pid_cmdline(int(entry.name)):
                    pids.append(int(entry.name))
            except OSError:
                continue
    return pids


def kill_recorded(task_dir: Path) -> list[str]:
    """Stop the processes setup recorded, and any left naming task_dir,
    only while they still name task_dir."""
    notes = []
    pidfile = task_dir / ".pids"
    recorded = [int(w) for w in pidfile.read_text().split()] if pidfile.exists() else []
    stray = [p for p in naming_dir(task_dir) if p not in recorded]
    if stray:
        notes.append(f"stopped processes naming the run's dir: {stray}")
    for pid in recorded + stray:
        if str(task_dir) not in pid_cmdline(pid):
            continue  # exited (or the PID was reused): nothing of ours to stop
        os.kill(pid, signal.SIGTERM)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and str(task_dir) in pid_cmdline(pid):
            time.sleep(0.1)
        if str(task_dir) in pid_cmdline(pid):
            os.kill(pid, signal.SIGKILL)
            notes.append(f"killed {pid} after SIGTERM timed out")
    return notes


def window_lines(mcp: Mcp, query: str) -> list[str]:
    """The windows lines (`'title' [app] at … {states} …`) matching query."""
    ok, text = mcp.call("windows", query=query)
    return [ln for ln in text.splitlines() if ln.startswith("'")] if ok else []


def is_active(line: str) -> bool:
    m = re.search(r"\{([^}]*)\}", line.rsplit("] ", 1)[-1])
    return bool(m) and "active" in m.group(1).split(",")


def other_windows(mcp: Mcp, app: str, tag: str) -> list[str]:
    """Titles of the windows of `app` that are not the task's (no tag)."""
    if not app:
        return []
    out = []
    for ln in window_lines(mcp, app):
        title = ln[1:ln.rfind("' [")] if "' [" in ln else ln
        if tag not in title:
            out.append(title)
    return out


def bring_to_front(mcp: Mcp, title: str) -> bool:
    """Activate the window titled `title` (a KWin call, no input) and
    confirm with a read that it is the active one."""
    for _ in range(3):
        mcp.call("activate_window", query=title)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if any(title in ln and is_active(ln) for ln in window_lines(mcp, title)):
                return True
            time.sleep(0.2)
    return False


def close_windows(mcp: Mcp, titles: list[str]) -> list[str]:
    """Close the task's windows of single-instance apps; retried for a few
    seconds (a window just opened may not be listed as an allowed app's
    yet, its app not registered with AT-SPI)."""
    notes = []
    for title in titles:
        deadline = time.monotonic() + 5
        while True:
            ok, text = mcp.call("windows", query=title)
            if not (ok and title in text):
                break  # gone (or never opened)
            ok, text = mcp.call("close_window", query=title, confirm=True)
            if ok and text.startswith("closed"):
                break
            if time.monotonic() >= deadline:
                notes.append(f"close_window {title!r}: {text.strip()}")
                break
            time.sleep(0.5)
    return notes


def mcp_config(binary: str, path: Path) -> Path:
    path.write_text(json.dumps({"mcpServers": {"desktop": {
        "type": "stdio", "command": binary, "args": []}}}))
    return path


# Flags every agent run gets, whatever --claude-args adds: no built-in tools
# (only the desktop tools exist), none of the user's settings, plugins, hooks
# or skills (the cwd is an empty temp dir, so project and local settings are
# empty too), and nothing outside --allowedTools may run.
ISOLATION = ["--tools", "", "--setting-sources", "project,local", "--disable-slash-commands",
             "--permission-mode", "dontAsk"]
# Seconds between SIGTERM and SIGKILL when stopping an agent.
KILL_GRACE = 3.0


def union_length(spans: list[tuple[float, float]]) -> float:
    """Total length covered by the [start, end] spans (overlaps once)."""
    total, reach = 0.0, float("-inf")
    for start, end in sorted(spans):
        if end <= reach:
            continue
        total += end - max(start, reach)
        reach = end
    return total


class Stream:
    """Measures a claude -p stream-json run from its events and their
    arrival times (seconds since the start)."""

    def __init__(self) -> None:
        self.m = {"model_used": None, "turns": 0, "tool_turns": 0, "tool_calls": 0,
                  "other_tool_calls": 0, "tool_errors": 0, "result_chars": 0, "images": 0,
                  "tool_s": 0.0, "startup_s": None, "end_s": None, "calls": [], "result": "",
                  "subtype": None, "init": {}}
        self.msg_ids: set = set()
        self.pending: dict[str, tuple[float, dict]] = {}
        self.spans: list[tuple[float, float]] = []
        self.final: dict = {}

    def feed(self, now: float, ev: dict) -> None:
        m = self.m
        kind = ev.get("type")
        if kind == "system" and ev.get("subtype") == "init":
            m["startup_s"] = now
            m["model_used"] = ev.get("model")
            m["init"] = {
                "model": ev.get("model"), "permission_mode": ev.get("permissionMode"),
                "plugins": [p.get("name") for p in ev.get("plugins", [])],
                "skills": len(ev.get("skills", [])),
                "slash_commands": len(ev.get("slash_commands", [])),
                "tools": ev.get("tools", []), "version": ev.get("claude_code_version")}
        elif kind == "assistant":
            msg = ev.get("message", {})
            if msg.get("id") not in self.msg_ids:
                self.msg_ids.add(msg.get("id"))
                m["turns"] += 1
            for block in msg.get("content", []):
                if block.get("type") != "tool_use":
                    continue
                name = block.get("name", "")
                call = {"tool": name.removeprefix(TOOL_PREFIX), "input": block.get("input"),
                        "turn": m["turns"]}
                if name.startswith(TOOL_PREFIX):
                    m["tool_calls"] += 1
                    if not any(c["turn"] == m["turns"] for c in m["calls"]):
                        m["tool_turns"] += 1
                else:
                    m["other_tool_calls"] += 1
                m["calls"].append(call)
                self.pending[block.get("id")] = (now, call)
        elif kind == "user":
            content = ev.get("message", {}).get("content", [])
            for block in content if isinstance(content, list) else []:
                if block.get("type") != "tool_result":
                    continue
                started, call = self.pending.pop(block.get("tool_use_id"), (now, {}))
                # Calls of one message run at once: tool_s counts the time
                # covered by any call, not the sum of their spans.
                self.spans.append((started, now))
                m["tool_s"] = union_length(self.spans)
                call["s"] = round(now - started, 3)
                parts = block.get("content")
                parts = [{"type": "text", "text": parts}] if isinstance(parts, str) else parts or []
                chars = sum(len(p.get("text", "")) for p in parts if p.get("type") == "text")
                imgs = sum(1 for p in parts if p.get("type") == "image")
                call.update(chars=chars, images=imgs, error=bool(block.get("is_error")))
                m["result_chars"] += chars
                m["images"] += imgs
                m["tool_errors"] += bool(block.get("is_error"))
        elif kind == "result":
            self.final = ev
            m["end_s"] = now

    def finish(self, exited_s: float) -> dict:
        """The measures, given when the process exited. Wall and model time
        end at the `result` event: the exit after it (Claude Code stopping
        the MCP server) is exit_s, apart from model time."""
        m, final = self.m, self.final
        end = m["end_s"] if m["end_s"] is not None else exited_s
        usage = final.get("usage", {})
        m.update(
            wall_s=end,
            exit_s=max(0.0, exited_s - end),
            api_s=(final.get("duration_api_ms") or 0) / 1000,
            model_s=max(0.0, end - (m["startup_s"] or 0) - m["tool_s"]),
            num_turns=final.get("num_turns"),
            subtype=final.get("subtype"),
            result=final.get("result") or "",
            cost_usd=final.get("total_cost_usd"),
            in_tokens=sum(usage.get(k, 0) for k in (
                "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")),
            out_tokens=usage.get("output_tokens", 0),
        )
        return m


def isolation_notes(init: dict) -> list[str]:
    """What the init event shows of the user's setup leaking into a run."""
    notes = []
    extra = [p for p in init.get("plugins", []) if not str(p).startswith("cc-plugin-")]
    if extra:
        notes.append(f"not isolated: plugins {extra}")
    if init.get("skills") or init.get("slash_commands"):
        notes.append(f"not isolated: {init.get('skills')} skills, "
                     f"{init.get('slash_commands')} slash commands")
    if any(not t.startswith(TOOL_PREFIX) for t in init.get("tools", [])):
        notes.append("not isolated: built-in tools present")
    return notes


def stop_group(proc: subprocess.Popen, grace: float | None = None) -> bool:
    """SIGTERM the process group, then SIGKILL after `grace` (default
    KILL_GRACE); whether it was still running."""
    if proc.poll() is not None:
        return False
    grace = KILL_GRACE if grace is None else grace
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(proc.pid, sig)
        except ProcessLookupError:
            break
        try:
            proc.wait(timeout=grace)
            break
        except subprocess.TimeoutExpired:
            continue
    return True


def agent_cmd(prompt: str, model: str, max_turns: int, config: Path, extra: list[str]) -> list[str]:
    cmd = ["claude", "-p", prompt, "--output-format", "stream-json", "--verbose",
           "--max-turns", str(max_turns), "--no-session-persistence",
           "--strict-mcp-config", "--mcp-config", str(config),
           "--allowedTools", "mcp__desktop", *ISOLATION, *extra]
    if model != "default":
        cmd[3:3] = ["--model", model]
    return cmd


def run_agent(prompt: str, model: str, max_turns: int, config: Path, cwd: Path,
              extra: list[str], log: Path, timeout: float) -> dict:
    """Run claude -p and measure its stream (times are when lines arrived).
    The agent's whole process group (claude and its desktop-mcp) is stopped
    on timeout and on any way out of here, Ctrl+C included."""
    errlog = log.with_suffix(".stderr")
    t0 = time.monotonic()
    with errlog.open("w", encoding="utf-8") as err:  # a file: a full pipe would block claude
        proc = subprocess.Popen(agent_cmd(prompt, model, max_turns, config, extra), cwd=cwd,
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=err,
                                text=True, encoding="utf-8", start_new_session=True)
    stream = Stream()
    timed_out = threading.Event()

    def expire() -> None:
        if proc.poll() is None:
            timed_out.set()
            stop_group(proc)

    timer = threading.Timer(timeout, expire)
    timer.daemon = True
    timer.start()
    try:
        with log.open("w", encoding="utf-8") as raw:
            assert proc.stdout is not None
            for line in proc.stdout:
                now = time.monotonic() - t0
                raw.write(f'{{"t": {now:.3f}, "event": {line.rstrip()}}}\n')
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                stream.feed(now, ev)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            stop_group(proc, grace=1.0)
    finally:
        timer.cancel()
        try:
            stop_group(proc)
        except KeyboardInterrupt:  # a second Ctrl+C: no grace period
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            raise
        finally:
            if proc.stdout:
                proc.stdout.close()
    m = stream.finish(time.monotonic() - t0)
    m.update(timed_out=timed_out.is_set(), exit_code=proc.returncode,
             stderr=errlog.read_text(encoding="utf-8", errors="replace")[-2000:])
    m["notes"] = isolation_notes(m["init"])
    return m


def run_check(check: dict, ns: dict, mcp: Mcp, env: dict, cwd: Path) -> tuple[bool, str]:
    def call(tool: str, **args) -> str:
        if tool not in READ_ONLY:
            raise ValueError(f"check may only call read-only tools, not {tool}")
        return mcp.call(tool, **args)[1]

    def read(path: str) -> str:
        return Path(path).read_text(encoding="utf-8")

    def lines(path: str) -> list[str]:
        return [ln.rstrip() for ln in read(path).rstrip("\n").split("\n")] if os.path.exists(path) else []

    scope = {"re": re, "os": os, "read": read, "lines": lines, "exists": os.path.exists,
             "mcp": call, **ns}
    try:
        if "python" in check:
            return bool(eval(check["python"], scope)), ""
        r = subprocess.run(["bash", "-c", check["shell"]], env=env, cwd=cwd,
                           stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=30)
        return r.returncode == 0, (r.stdout + r.stderr).strip()[-500:]
    except Exception as e:  # a failing predicate is a failed task, not a crash
        return False, f"{type(e).__name__}: {e}"


def one_run(task: dict, model: str, label: str, binary: str, args, work: Path,
            results: Path, rep: int) -> dict:
    task_dir = Path(tempfile.mkdtemp(prefix=f"{task['id']}-", dir=work))
    tag = f"e2e{random.randrange(16**6):06x}"
    subs = {"dir": str(task_dir), "tag": tag, "num": str(random.randrange(1000, 99999))}
    env = {**os.environ, "TASK_DIR": subs["dir"], "TAG": tag, "NUM": subs["num"]}
    for name, content in task.get("files", {}).items():
        (task_dir / fill(name, subs)).write_text(fill(content, subs), encoding="utf-8")
    row = {"task": task["id"], "model": model, "server": label, "binary": binary, "rep": rep,
           "tag": tag, "dir": str(task_dir), "ok": False, "notes": []}
    mcp = Mcp(binary)
    try:
        # Before setup: a skipped run must open nothing.
        if args.exclusive:
            others = other_windows(mcp, task.get("app", ""), tag)
            if others:
                row["notes"] += [f"other {task['app']} windows open: {others}",
                                 "--exclusive: not run"]
                return row
        run_shell(task.get("setup", []), env, task_dir, "setup")
        if "wait_window" in task:
            title = fill(task["wait_window"], subs)
            if not wait_window(mcp, title, 30):
                row["notes"].append("setup window never appeared; agent not run")
                return row
            others = other_windows(mcp, task.get("app", ""), tag)
            if others:
                row["notes"].append(f"other {task['app']} windows open: {others}")
                if args.exclusive:
                    row["notes"].append("--exclusive: agent not run")
                    return row
            time.sleep(args.settle)
            # The agent starts with the task's window in front, not the
            # terminal or another window of the same (single-instance) app.
            if not bring_to_front(mcp, title):
                row["notes"].append(f"could not make {title!r} the active window; agent not run")
                return row
        else:
            time.sleep(args.settle)
        stem = f"{task['id']}-{model}-{label}-{rep}".replace("/", "_")
        config = mcp_config(binary, results / f"{stem}.mcp.json")
        m = run_agent(fill(task["prompt"], subs), model, task.get("max_turns", args.max_turns),
                      config, work, args.claude_args, results / f"{stem}.jsonl", args.timeout)
        notes = m.pop("notes")
        row.update(m)
        row["notes"] += notes
        ns = {"dir": subs["dir"], "tag": tag, "num": subs["num"], "result": m["result"]}
        deadline = time.monotonic() + args.check_grace
        while True:  # a save may land a moment after the agent's last reply
            ok, detail = run_check(task["check"], ns, mcp, env, task_dir)
            if ok or time.monotonic() >= deadline:
                break
            time.sleep(0.5)
        row["ok"] = ok
        if detail:
            row["notes"].append(f"check: {detail}")
    finally:
        try:
            run_shell(task.get("cleanup", []), env, task_dir, "cleanup")
        except Exception as e:
            row["notes"].append(str(e))
        row["notes"] += close_windows(mcp, [fill(t, subs) for t in task.get("close_windows", [])])
        row["notes"] += kill_recorded(task_dir)
        mcp.close()
        if not args.keep:
            shutil.rmtree(task_dir, ignore_errors=True)
    return row


def med(values: list) -> float | None:
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def fmt(v, digits: int = 1) -> str:
    if v is None:
        return "-"
    if isinstance(v, float) and not v.is_integer():
        return f"{v:.{digits}f}"
    return f"{int(v)}"


def table(rows: list[dict]) -> str:
    cols = ["task", "model", "model used", "server", "ok", "turns", "calls", "wall s",
            "startup s", "model s", "api s", "tool s", "exit s", "reply chars", "images",
            "in tok", "out tok"]
    out = ["| " + " | ".join(cols) + " |",
           "|" + "|".join(["---"] * 4 + ["---:"] * (len(cols) - 4)) + "|"]
    groups: dict[tuple, list[dict]] = {}
    for r in rows:
        groups.setdefault((r["task"], r["model"], r["server"]), []).append(r)
    for (task, model, server), rs in groups.items():
        g = lambda k: med([r.get(k) for r in rs])  # noqa: E731
        used = ",".join(sorted({r["model_used"] for r in rs if r.get("model_used")})) or "-"
        out.append("| " + " | ".join([
            task, model, used, server, f"{sum(r['ok'] for r in rs)}/{len(rs)}",
            fmt(g("turns")), fmt(g("tool_calls")), fmt(g("wall_s")), fmt(g("startup_s")),
            fmt(g("model_s")), fmt(g("api_s")), fmt(g("tool_s"), 2), fmt(g("exit_s"), 2),
            fmt(g("result_chars"), 0), fmt(g("images")), fmt(g("in_tokens"), 0),
            fmt(g("out_tokens"), 0)]) + " |")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tasks", default="", help="comma-separated task ids (default: all)")
    ap.add_argument("--task-file", default=str(DEFAULT_TASKS))
    ap.add_argument("--models", default=",".join(DEFAULT_MODELS),
                    help="comma-separated; 'default' = no --model: Claude Code's own default "
                         "for the account (user settings are not loaded); the table's "
                         "'model used' column names it")
    ap.add_argument("--server", action="append", default=[],
                    help=f"label=binary, repeatable (default: dev={DEFAULT_SERVER})")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--max-turns", type=int, default=20, help="unless the task sets max_turns")
    ap.add_argument("--timeout", type=float, default=600, help="seconds per agent run")
    ap.add_argument("--settle", type=float, default=1.0,
                    help="seconds between the setup window appearing and the agent start")
    ap.add_argument("--check-grace", type=float, default=3.0,
                    help="seconds to retry a failing check")
    ap.add_argument("--results", default="", help="results dir (default: a new temp dir)")
    ap.add_argument("--keep", action="store_true", help="keep the task temp dirs")
    ap.add_argument("--claude-args", default="",
                    help="extra claude CLI args (e.g. '--effort low'), added after the "
                         "fixed ones: " + shlex.join(ISOLATION))
    ap.add_argument("--exclusive", action="store_true",
                    help="skip a run when other windows of the task's app are open "
                         "(default: run, with a note)")
    ap.add_argument("--list", action="store_true", help="list the tasks and exit")
    args = ap.parse_args()
    args.claude_args = shlex.split(args.claude_args)

    tasks = json.loads(Path(args.task_file).read_text(encoding="utf-8"))["tasks"]
    if args.list:
        for t in tasks:
            print(f"{t['id']:16} {t.get('summary', '')}")
        return 0
    if args.tasks:
        wanted = args.tasks.split(",")
        unknown = set(wanted) - {t["id"] for t in tasks}
        if unknown:
            ap.error(f"unknown task(s): {', '.join(sorted(unknown))}")
        tasks = [t for t in tasks if t["id"] in wanted]
    servers = []
    for spec in args.server or [f"dev={DEFAULT_SERVER}"]:
        label, sep, binary = spec.partition("=")
        if not sep:
            ap.error(f"--server wants label=binary, got {spec!r}")
        binary = str(Path(binary).expanduser().resolve())
        if not os.access(binary, os.X_OK):
            ap.error(f"not an executable: {binary}")
        servers.append((label, binary))
    models = [m for m in args.models.split(",") if m]

    results = Path(args.results).expanduser().resolve() if args.results else \
        Path(tempfile.mkdtemp(prefix="desktop-mcp-e2e-"))
    if results == REPO or REPO in results.parents:
        ap.error(f"--results must be outside the repository ({REPO})")
    results.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="desktop-mcp-e2e-work-"))
    print(f"results: {results}", file=sys.stderr)

    rows = []
    try:
        for task in tasks:
            for model in models:
                for rep in range(1, args.repeat + 1):
                    for label, binary in servers:  # servers inner: drift hits all alike
                        print(f"[{task['id']} | {model} | {label} | {rep}/{args.repeat}] ...",
                              file=sys.stderr, flush=True)
                        try:
                            row = one_run(task, model, label, binary, args, work, results, rep)
                        except Exception as e:
                            row = {"task": task["id"], "model": model, "server": label,
                                   "binary": binary, "rep": rep, "ok": False,
                                   "notes": [f"{type(e).__name__}: {e}"]}
                        rows.append(row)
                        print(f"  ok={row['ok']} turns={row.get('turns')} "
                              f"calls={row.get('tool_calls')} wall={fmt(row.get('wall_s'))}s"
                              + (f" notes={row['notes']}" if row["notes"] else ""),
                              file=sys.stderr, flush=True)
                        (results / "results.json").write_text(
                            json.dumps(rows, indent=1, ensure_ascii=False), encoding="utf-8")
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)

    md = table(rows)
    (results / "table.md").write_text(md + "\n", encoding="utf-8")
    print(md)
    print(f"\nresults: {results}")
    return 0 if all(r["ok"] for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
