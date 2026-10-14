"""Hyprland backend logic with recorded-shape hyprctl output and fake AT-SPI.

Needs the Linux dependencies (PyGObject, evdev) but no Hyprland session.
Run: .venv/bin/python -m unittest discover tests
"""

from __future__ import annotations

import sys
import types
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if not sys.platform.startswith("linux"):
    raise unittest.SkipTest("Linux backend tests")

from backends import linux_hyprland as hl  # noqa: E402
import backends  # noqa: E402

KATE = {"address": "0x55d1a0", "mapped": True, "hidden": False, "at": [100, 50],
        "size": [800, 600], "class": "org.kde.kate", "title": "notes.txt — Kate",
        "pid": 4242, "xwayland": False}
KATE2 = dict(KATE, address="0x55d1b0", title="other.txt — Kate", at=[960, 50])
XTERM = {"address": "0x55d1c0", "mapped": True, "hidden": False, "at": [0, 0],
         "size": [640, 480], "class": "XTerm", "title": "xterm", "pid": 5151,
         "xwayland": True}
FIREFOX = {"address": "0x55d1d0", "mapped": True, "hidden": False, "at": [0, 0],
           "size": [1920, 1080], "class": "firefox", "title": "YouTube — Mozilla Firefox",
           "pid": 6161, "xwayland": False}


class FakeAcc:
    def __init__(self, name, role="frame", pid=0, kids=(), parent=None, app=None):
        self._name, self._role, self._pid = name, role, pid
        self._kids, self._parent, self._app = list(kids), parent, app

    def get_name(self): return self._name
    def get_role_name(self): return self._role
    def get_process_id(self): return self._pid
    def get_child_count(self): return len(self._kids)
    def get_child_at_index(self, i): return self._kids[i]
    def get_parent(self): return self._parent
    def get_application(self): return self._app


def make_app(name, pid, titles):
    app = FakeAcc(name, role="application", pid=pid)
    frames = [FakeAcc(t, role="frame", parent=app, app=app) for t in titles]
    app._kids = frames
    return app, frames


class HyprlandTest(unittest.TestCase):
    def setUp(self):
        self.kate, self.kate_frames = make_app("kate", 4242,
                                               ["notes.txt — Kate", "other.txt — Kate"])
        self.firefox, _ = make_app("Firefox", 6161, ["YouTube — Mozilla Firefox"])
        self.b = hl.HyprlandBackend()
        self.b._applications = lambda: [self.kate, self.firefox]
        self.active = KATE
        self.clients = [KATE, KATE2, XTERM, FIREFOX]
        self.dispatched = []

        def fake_json(*args):
            if args == ("activewindow",):
                return self.active
            if args == ("clients",):
                return self.clients
            raise AssertionError(args)

        patches = [
            mock.patch.object(hl, "_hyprctl_json", side_effect=fake_json),
            mock.patch.object(hl, "_hyprctl_dispatch",
                              side_effect=lambda *a: self.dispatched.append(a)),
            mock.patch.object(hl.time, "sleep"),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def test_active_app_maps_pid_to_atspi_name(self):
        self.assertEqual(self.b.active_app(), "kate")
        self.active = XTERM  # no accessibility: fall back to window class
        self.assertEqual(self.b.active_app(), "xterm")
        self.active = {}
        self.assertEqual(self.b.active_app(), "")

    def test_window_active_matches_title_when_app_has_several_windows(self):
        self.assertTrue(self.b._window_active(self.kate, self.kate_frames[0]))
        self.assertFalse(self.b._window_active(self.kate, self.kate_frames[1]))
        self.assertFalse(self.b._window_active(self.firefox, self.firefox._kids[0]))

    def test_activate_window_respects_allowlist_and_query(self):
        self.b.activate_window("other.txt", {"kate"})
        self.assertEqual(self.dispatched, [("focuswindow", "address:0x55d1b0")])
        self.dispatched.clear()
        self.b.activate_window("youtube", {"kate"})  # firefox not allowed
        self.assertEqual(self.dispatched, [])
        self.b.activate_window("xterm", {"xterm"})  # class fallback name
        self.assertEqual(self.dispatched, [("focuswindow", "address:0x55d1c0")])

    def test_element_center_adds_window_origin_for_wayland_clients(self):
        child = FakeAcc("Kaydet", role="push button", parent=self.kate_frames[0], app=self.kate)
        ext = types.SimpleNamespace(x=20, y=30, width=40, height=10)
        with mock.patch.object(hl, "window_extents", return_value=ext):
            self.assertEqual(self.b.element_center(types.SimpleNamespace(acc=child)),
                             (100 + 20 + 20, 50 + 30 + 5))

    def test_element_center_refuses_inactive_window(self):
        child = FakeAcc("x", role="push button", parent=self.kate_frames[1], app=self.kate)
        with self.assertRaises(RuntimeError):
            self.b.element_center(types.SimpleNamespace(acc=child))

    def test_element_center_uses_screen_coords_for_xwayland(self):
        xapp, frames = make_app("xterm", 5151, ["xterm"])
        self.active = XTERM
        child = FakeAcc("x", role="text", parent=frames[0], app=xapp)
        ext = types.SimpleNamespace(x=300, y=200, width=10, height=20)
        with mock.patch.object(hl, "screen_extents", return_value=ext):
            self.assertEqual(self.b.element_center(types.SimpleNamespace(acc=child)), (305, 210))

    def test_pointer_moves_cursor_with_hyprctl_then_clicks(self):
        dev = mock.MagicMock()
        self.b._btn = dev
        self.b.pointer(140, 85, "left", clicks=2, scroll=3)
        self.assertEqual(self.dispatched, [("movecursor", "140", "85")])
        presses = [c.args for c in dev.write.call_args_list if c.args[0] == hl.e.EV_KEY]
        self.assertEqual(presses, [(hl.e.EV_KEY, hl.e.BTN_LEFT, 1), (hl.e.EV_KEY, hl.e.BTN_LEFT, 0)] * 2)
        dev.write.assert_any_call(hl.e.EV_REL, hl.e.REL_WHEEL, -3)

    def test_wtype_commands_send_newlines_and_tabs_as_keys(self):
        self.assertEqual(hl._wtype_commands("ab\nçğ\tı"), [
            ["wtype", "--", "ab"], ["wtype", "-k", "Return"],
            ["wtype", "--", "çğ"], ["wtype", "-k", "Tab"], ["wtype", "--", "ı"]])
        self.assertEqual(hl._wtype_commands("\n"), [["wtype", "-k", "Return"]])

    def test_screenshot_uses_active_window_geometry(self):
        with mock.patch.object(hl.shutil, "which", return_value="/usr/bin/grim"), \
             mock.patch.object(hl.subprocess, "run",
                               return_value=types.SimpleNamespace(returncode=0)) as run:
            path = self.b.screenshot()
        self.assertTrue(path.endswith(".png"))
        Path(path).unlink()
        self.assertEqual(run.call_args.args[0][:3], ["grim", "-g", "100,50 800x600"])


class DetectTest(unittest.TestCase):
    def test_detect(self):
        with mock.patch.dict("os.environ", {"HYPRLAND_INSTANCE_SIGNATURE": "abc"}, clear=True):
            self.assertEqual(backends.detect(), "hyprland")
        with mock.patch.dict("os.environ", {}, clear=True):
            self.assertEqual(backends.detect(), "kde")
        with mock.patch.dict("os.environ", {"DESKTOP_MCP_BACKEND": "KDE",
                                            "HYPRLAND_INSTANCE_SIGNATURE": "abc"}, clear=True):
            self.assertEqual(backends.detect(), "kde")


if __name__ == "__main__":
    unittest.main()
