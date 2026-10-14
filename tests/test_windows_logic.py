"""Windows backend logic with fake UI Automation and Win32 (runs on Linux/macOS).

Covers key mapping, Unicode typing, app grouping, tree states/actions and focus.
On real Windows use tools/smoke_test.py instead.
"""

from __future__ import annotations

import ctypes
import sys
import types
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if sys.platform == "win32":
    raise unittest.SkipTest("uses fakes for Win32; run tools/smoke_test.py on Windows")


class _FakeDLL:
    def __getattr__(self, name):
        return types.SimpleNamespace()


def _import_windows_backend():
    saved = {k: getattr(ctypes, k, None) for k in ("WinDLL", "get_last_error")}
    ctypes.WinDLL = lambda *a, **k: _FakeDLL()
    ctypes.get_last_error = lambda: 0
    auto = types.ModuleType("uiautomation")
    auto.Control = object
    auto.PatternId = types.SimpleNamespace(
        ValuePattern=1, TextPattern=2, TogglePattern=3, SelectionItemPattern=4,
        ExpandCollapsePattern=5, InvokePattern=6, LegacyIAccessiblePattern=7)
    auto.ToggleState = types.SimpleNamespace(On=1)
    auto.ExpandCollapseState = types.SimpleNamespace(Expanded=1, PartiallyExpanded=2)
    sys.modules["uiautomation"] = auto
    try:
        from backends import windows
    finally:
        for k, v in saved.items():
            if v is None:
                delattr(ctypes, k)
            else:
                setattr(ctypes, k, v)
    return windows, auto


w, auto = _import_windows_backend()


class Range:
    def __init__(self, text):
        self.text = text

    def GetText(self, n):
        return self.text if n < 0 else self.text[:n]


class Ctrl:
    def __init__(self, typ, name, pid, hwnd=0, kids=(), patterns=None):
        self.ControlTypeName, self.Name, self.ProcessId = typ, name, pid
        self.NativeWindowHandle, self.kids, self.patterns = hwnd, list(kids), patterns or {}
        self.HasKeyboardFocus, self.IsOffscreen = False, False
        self.IsEnabled, self.IsKeyboardFocusable = True, True

    def GetChildren(self):
        return self.kids

    def GetPattern(self, pid):
        return self.patterns.get(pid)

    def SetFocus(self):
        self.HasKeyboardFocus = True
        return True


class InputTest(unittest.TestCase):
    def setUp(self):
        self.sent = []
        self._orig_send = w._send
        w._send = self.sent.extend
        self.b = w.WindowsBackend.__new__(w.WindowsBackend)

    def tearDown(self):
        w._send = self._orig_send

    def test_key_names(self):
        self.assertEqual([w._vk(k) for k in ("ctrl", "S", "f5", "end")], [0x11, 0x53, 0x74, 0x23])
        with self.assertRaises(ValueError):
            w._vk("ğ")

    def test_chord_order_and_extended_flag(self):
        self.b.press_combo("ctrl+shift+end")
        self.assertEqual([(i.ki.wVk, i.ki.dwFlags) for i in self.sent],
                         [(0x11, 0), (0x10, 0), (0x23, 1), (0x23, 3), (0x10, 2), (0x11, 2)])

    def test_unicode_typing_with_surrogates_newline_tab(self):
        self.b.type_text("aİ\r\n😀\t")
        self.assertEqual([(i.ki.wVk, i.ki.wScan, i.ki.dwFlags) for i in self.sent], [
            (0, 0x61, 4), (0, 0x61, 6), (0, 0x130, 4), (0, 0x130, 6),
            (0x0D, 0, 0), (0x0D, 0, 2),
            (0, 0xD83D, 4), (0, 0xD83D, 6), (0, 0xDE00, 4), (0, 0xDE00, 6),
            (0x09, 0, 0), (0x09, 0, 2)])


class TreeTest(unittest.TestCase):
    def setUp(self):
        self.calls = []
        invoke = types.SimpleNamespace(Invoke=lambda waitTime: self.calls.append("inv") or True)
        self.btn = Ctrl("ButtonControl", "Kaydet", 10, patterns={6: invoke})
        self.doc = Ctrl("DocumentControl", "Metin", 10,
                        patterns={2: types.SimpleNamespace(DocumentRange=Range("ilk satır\r\nikinci"))})
        win1 = Ctrl("WindowControl", "a.txt - Notepad", 10, hwnd=100, kids=[self.btn, self.doc])
        win2 = Ctrl("WindowControl", "b.txt - Notepad", 11, hwnd=200)
        other = Ctrl("WindowControl", "Firefox", 12, hwnd=300)
        self.saved = (w._process_name, w._foreground_hwnd, w._hwnd_process_name)
        auto.GetRootControl = lambda: types.SimpleNamespace(GetChildren=lambda: [win1, other, win2])
        w._process_name = lambda pid: {10: "notepad", 11: "notepad", 12: "firefox"}[pid]
        w._foreground_hwnd = lambda: 100
        w._hwnd_process_name = lambda h: {100: "notepad"}.get(h, "")
        self.b = w.WindowsBackend()

    def tearDown(self):
        w._process_name, w._foreground_hwnd, w._hwnd_process_name = self.saved

    def test_apps_group_processes_and_mark_active(self):
        apps = {a.name: a for a in self.b.list_apps()}
        self.assertEqual([x.title for x in apps["notepad"].windows],
                         ["a.txt - Notepad", "b.txt - Notepad"])
        self.assertEqual([x.active for x in apps["notepad"].windows], [True, False])
        self.assertEqual(self.b.active_app(), "notepad")
        self.assertIsNone(self.b.app_root("chrome"))

    def test_nodes(self):
        win_a, win_b = self.b.app_root("Notepad").children()
        self.assertIn("active", win_a.states())
        self.assertNotIn("active", win_b.states())
        btn, doc = win_a.children()
        self.assertEqual((btn.role(), btn.actions()), ("button", ["invoke"]))
        self.assertTrue(btn.do_action(0))
        self.assertEqual(self.calls, ["inv"])
        self.assertEqual(doc.role(), "document")
        self.assertIn("editable", doc.states())
        self.assertEqual(doc.full_text(-1), "ilk satır\nikinci")
        self.assertEqual(self.b.try_focus(doc), "SetFocus")
        with self.assertRaises(ValueError):
            doc.set_text("x")


if __name__ == "__main__":
    unittest.main()
