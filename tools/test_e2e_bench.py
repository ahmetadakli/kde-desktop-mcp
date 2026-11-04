"""Unit tests for tools/e2e_bench.py (no desktop, no real claude):

    python3 -m unittest tools/test_e2e_bench.py
"""

from __future__ import annotations

import json
import os
import signal
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import e2e_bench as b  # noqa: E402


def tool_use(msg_id: str, *ids: str) -> dict:
    return {"type": "assistant", "message": {"id": msg_id, "content": [
        {"type": "tool_use", "id": i, "name": "mcp__desktop__find", "input": {}} for i in ids]}}


def tool_result(tid: str, text: str = "ok") -> dict:
    return {"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": tid, "content": [{"type": "text", "text": text}]}]}}


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    # A zombie still answers kill(0); it no longer runs anything.
    try:
        return Path(f"/proc/{pid}/stat").read_text().split(")")[-1].split()[0] != "Z"
    except OSError:
        return False


class UnionTest(unittest.TestCase):
    def test_union(self):
        self.assertEqual(b.union_length([]), 0)
        self.assertAlmostEqual(b.union_length([(0, 1), (2, 3)]), 2)
        self.assertAlmostEqual(b.union_length([(0, 2), (1, 3)]), 3)
        self.assertAlmostEqual(b.union_length([(0, 4), (1, 2)]), 4)
        self.assertAlmostEqual(b.union_length([(5, 6), (0, 1), (0.5, 1.5)]), 2.5)


class StreamTest(unittest.TestCase):
    def run_stream(self, events, exited):
        s = b.Stream()
        for t, ev in events:
            s.feed(t, ev)
        return s.finish(exited)

    def test_parallel_calls_count_once(self):
        # One message with two tool_use blocks: the calls overlap
        # (1.0-1.5 and 1.1-1.6), so tool time is 0.6 s, not 1.0 s.
        m = self.run_stream([
            (0.5, {"type": "system", "subtype": "init", "model": "m"}),
            (1.0, tool_use("a", "t1")),
            (1.1, tool_use("a", "t2")),
            (1.5, tool_result("t1", "abc")),
            (1.6, tool_result("t2", "de")),
            (3.0, tool_use("b", "t3")),
            (3.2, tool_result("t3")),
            (4.0, {"type": "result", "subtype": "success", "num_turns": 3,
                   "duration_api_ms": 2500, "result": "done"}),
        ], exited=4.9)
        self.assertEqual(m["turns"], 2)
        self.assertEqual(m["tool_turns"], 2)
        self.assertEqual(m["tool_calls"], 3)
        self.assertAlmostEqual(m["tool_s"], 0.8)
        self.assertEqual(m["result_chars"], 7)
        # Wall and model time end at the result; the exit is apart.
        self.assertAlmostEqual(m["wall_s"], 4.0)
        self.assertAlmostEqual(m["exit_s"], 0.9)
        self.assertAlmostEqual(m["model_s"], 4.0 - 0.5 - 0.8)
        self.assertEqual(m["result"], "done")
        self.assertEqual([c["s"] for c in m["calls"]], [0.5, 0.5, 0.2])

    def test_no_result_ends_at_exit(self):
        m = self.run_stream([(0.5, {"type": "system", "subtype": "init"})], exited=2.0)
        self.assertAlmostEqual(m["wall_s"], 2.0)
        self.assertEqual(m["exit_s"], 0)

    def test_init_and_isolation_notes(self):
        m = self.run_stream([(0.1, {
            "type": "system", "subtype": "init", "model": "x", "permissionMode": "dontAsk",
            "plugins": [{"name": "cc-plugin-telemetry"}], "skills": [], "slash_commands": [],
            "tools": ["mcp__desktop__find"]})], exited=1)
        self.assertEqual(m["init"]["plugins"], ["cc-plugin-telemetry"])
        self.assertEqual(b.isolation_notes(m["init"]), [])
        leaky = {"plugins": ["cc-plugin-x", "is-takibi"], "skills": 30, "slash_commands": 69,
                 "tools": ["Bash", "mcp__desktop__find"]}
        notes = b.isolation_notes(leaky)
        self.assertEqual(len(notes), 3)
        self.assertIn("is-takibi", notes[0])


class CommandTest(unittest.TestCase):
    def test_isolation_always_present(self):
        cmd = b.agent_cmd("p", "default", 5, Path("/c.json"), ["--effort", "low"])
        self.assertNotIn("--model", cmd)
        i = cmd.index("--tools")
        self.assertEqual(cmd[i + 1], "")
        for flag in ("--setting-sources", "--disable-slash-commands", "--permission-mode"):
            self.assertIn(flag, cmd)
        self.assertEqual(cmd[-2:], ["--effort", "low"])
        cmd = b.agent_cmd("p", "claude-haiku-5-5", 5, Path("/c.json"), [])
        self.assertEqual(cmd[cmd.index("--model") + 1], "claude-haiku-5-5")


class WindowsTest(unittest.TestCase):
    LINES = textwrap.dedent("""\
        screen eDP-1: at (1920,0) 1536x864, scale 1.25
        desktops: 1 'Desk' (current)
        'Sign-up e2eabc — Mozilla Firefox' [firefox] at (0,0) 800x600 {active,maximized} on eDP-1, desktop 1
        'YouTube [live] {x} — Mozilla Firefox' [firefox] at (0,0) 800x600 {maximized} on eDP-1, desktop 1
        """)

    def fake(self, text):
        mcp = mock.Mock()
        mcp.call.return_value = (True, text)
        return mcp

    def test_active_and_others(self):
        lines = b.window_lines(self.fake(self.LINES), "firefox")
        self.assertEqual(len(lines), 2)
        self.assertTrue(b.is_active(lines[0]))
        self.assertFalse(b.is_active(lines[1]))
        self.assertEqual(b.other_windows(self.fake(self.LINES), "firefox", "e2eabc"),
                         ["YouTube [live] {x} — Mozilla Firefox"])
        self.assertEqual(b.other_windows(self.fake(self.LINES), "", "e2eabc"), [])


FAKE_CLAUDE = """#!/usr/bin/env python3
import json, os, signal, sys, time, subprocess
mode = os.environ["FAKE_MODE"]
# A child in the same group that ignores SIGTERM, like a stuck MCP server
# holding the stdout pipe.
child = subprocess.Popen([sys.executable, "-c",
    "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"])
open(os.environ["FAKE_PIDS"], "w").write(f"{os.getpid()} {child.pid}")
signal.signal(signal.SIGTERM, signal.SIG_IGN)
def out(ev):
    print(json.dumps(ev), flush=True)
out({"type": "system", "subtype": "init", "model": "fake"})
if mode == "finish":
    out({"type": "result", "subtype": "success", "result": "hi", "num_turns": 1})
    child.kill()
    time.sleep(0.4)
    sys.exit(0)
time.sleep(60)
"""


class RunAgentTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="e2e-bench-test-"))
        fake = self.tmp / "claude"
        fake.write_text(FAKE_CLAUDE)
        fake.chmod(0o755)
        self.pids = self.tmp / "pids"
        self.env = mock.patch.dict(os.environ, {
            "PATH": f"{self.tmp}:{os.environ['PATH']}", "FAKE_PIDS": str(self.pids)})
        self.env.start()
        self.grace = mock.patch.object(b, "KILL_GRACE", 0.5)
        self.grace.start()

    def tearDown(self):
        self.grace.stop()
        self.env.stop()
        if self.pids.exists():
            for pid in map(int, self.pids.read_text().split()):
                if alive(pid):
                    os.kill(pid, signal.SIGKILL)

    def run_fake(self, mode, timeout):
        os.environ["FAKE_MODE"] = mode
        return b.run_agent("p", "default", 3, self.tmp / "c.json", self.tmp, [],
                           self.tmp / "log.jsonl", timeout)

    def assert_all_gone(self):
        pids = list(map(int, self.pids.read_text().split()))
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and any(alive(p) for p in pids):
            time.sleep(0.05)
        self.assertFalse([p for p in pids if alive(p)])

    def test_finish_measures_exit_apart(self):
        m = self.run_fake("finish", timeout=30)
        self.assertFalse(m["timed_out"])
        self.assertEqual(m["result"], "hi")
        self.assertGreaterEqual(m["exit_s"], 0.3)  # the 0.4 s after the result
        self.assertIsNotNone(m["end_s"])
        self.assertAlmostEqual(m["wall_s"], m["end_s"])

    def test_timeout_kills_group_despite_sigterm(self):
        t0 = time.monotonic()
        m = self.run_fake("hang", timeout=1.0)
        self.assertTrue(m["timed_out"])
        self.assertLess(time.monotonic() - t0, 10)
        self.assert_all_gone()

    def test_interrupt_kills_group(self):
        calls = {"n": 0}
        real = b.Stream.feed

        def feed(self, now, ev):
            real(self, now, ev)
            calls["n"] += 1
            raise KeyboardInterrupt

        with mock.patch.object(b.Stream, "feed", feed):
            with self.assertRaises(KeyboardInterrupt):
                self.run_fake("hang", timeout=60)
        self.assertEqual(calls["n"], 1)
        self.assert_all_gone()


if __name__ == "__main__":
    unittest.main()
