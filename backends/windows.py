"""Windows backend: UI Automation (via `uiautomation`) and Win32 SendInput.

App names are executable names without ".exe" (e.g. "notepad", "firefox").
Coordinates are physical screen pixels: `uiautomation` makes the process
per-monitor DPI aware on import, so UIA bounds and SetCursorPos agree.
"""

from __future__ import annotations

import ctypes
import os
import tempfile
import time
from ctypes import wintypes
from pathlib import Path

import uiautomation as auto

from .base import AppGroupNode, AppInfo, Backend, Node, WindowInfo

DEFAULT_ALLOW = """\
# One app (executable name without .exe) per line, case-insensitive.
# See `apps` tool output. Keyboard/mouse input is only sent when the
# foreground window belongs to one of these.
notepad
firefox
chrome
msedge
winword
excel
"""

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

# ---------------------------------------------------------------- Win32 input

ULONG_PTR = ctypes.c_size_t
INPUT_MOUSE, INPUT_KEYBOARD = 0, 1
KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE = 0x1, 0x2, 0x4
MOUSE_FLAGS = {
    "left": (0x0002, 0x0004),
    "right": (0x0008, 0x0010),
    "middle": (0x0020, 0x0040),
}
MOUSEEVENTF_WHEEL = 0x0800
WHEEL_DELTA = 120


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG),
                ("mouseData", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                ("dwExtraInfo", ULONG_PTR)]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [("uMsg", wintypes.DWORD), ("wParamL", wintypes.WORD),
                ("wParamH", wintypes.WORD)]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


user32.SendInput.argtypes = (wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int)
user32.SendInput.restype = wintypes.UINT
user32.GetForegroundWindow.restype = wintypes.HWND
user32.GetWindowThreadProcessId.argtypes = (wintypes.HWND, ctypes.POINTER(wintypes.DWORD))
user32.SetForegroundWindow.argtypes = (wintypes.HWND,)
user32.ShowWindow.argtypes = (wintypes.HWND, ctypes.c_int)
user32.IsIconic.argtypes = (wintypes.HWND,)
user32.SetCursorPos.argtypes = (ctypes.c_int, ctypes.c_int)
user32.GetWindowRect.argtypes = (wintypes.HWND, ctypes.POINTER(wintypes.RECT))
kernel32.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
kernel32.OpenProcess.restype = wintypes.HANDLE
kernel32.QueryFullProcessImageNameW.argtypes = (
    wintypes.HANDLE, wintypes.DWORD, wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD))
kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)


def _send(inputs: list[INPUT]) -> None:
    if not inputs:
        return
    arr = (INPUT * len(inputs))(*inputs)
    sent = user32.SendInput(len(inputs), arr, ctypes.sizeof(INPUT))
    if sent != len(inputs):
        raise OSError(ctypes.get_last_error(),
                      "SendInput was blocked (a higher-integrity window may be in front)")


def _key(vk: int = 0, scan: int = 0, flags: int = 0) -> INPUT:
    return INPUT(type=INPUT_KEYBOARD, ki=KEYBDINPUT(vk, scan, flags, 0, 0))


def _mouse(flags: int, data: int = 0) -> INPUT:
    return INPUT(type=INPUT_MOUSE,
                 mi=MOUSEINPUT(0, 0, ctypes.c_uint32(data).value, flags, 0, 0))


VK = {
    "ctrl": 0x11, "control": 0x11, "shift": 0x10, "alt": 0x12, "altgr": 0xA5,
    "win": 0x5B, "super": 0x5B, "meta": 0x5B, "enter": 0x0D, "return": 0x0D,
    "esc": 0x1B, "escape": 0x1B, "tab": 0x09, "space": 0x20, "backspace": 0x08,
    "delete": 0x2E, "del": 0x2E, "insert": 0x2D, "home": 0x24, "end": 0x23,
    "pageup": 0x21, "pgup": 0x21, "pagedown": 0x22, "pgdn": 0x22,
    "left": 0x25, "up": 0x26, "right": 0x27, "down": 0x28,
    "capslock": 0x14, "printscreen": 0x2C, "sysrq": 0x2C, "menu": 0x5D,
    "compose": 0x5D,
}
EXTENDED = {0x2E, 0x2D, 0x24, 0x23, 0x21, 0x22, 0x25, 0x26, 0x27, 0x28,
            0x5B, 0x5D, 0xA5, 0x2C}


def _vk(name: str) -> int:
    n = name.lower()
    if n in VK:
        return VK[n]
    if len(n) == 1 and (n.isascii() and n.isalnum()):
        return ord(n.upper())
    if n.startswith("f") and n[1:].isdigit() and 1 <= int(n[1:]) <= 24:
        return 0x70 + int(n[1:]) - 1
    raise ValueError(f"unknown key '{name}'")


def _key_flags(vk: int, up: bool) -> int:
    return (KEYEVENTF_EXTENDEDKEY if vk in EXTENDED else 0) | (KEYEVENTF_KEYUP if up else 0)


# ---------------------------------------------------------------- processes

_proc_names: dict[int, str] = {}


def _process_name(pid: int) -> str:
    if pid in _proc_names:
        return _proc_names[pid]
    name = ""
    h = kernel32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if h:
        try:
            buf = ctypes.create_unicode_buffer(1024)
            size = wintypes.DWORD(len(buf))
            if kernel32.QueryFullProcessImageNameW(h, 0, buf, ctypes.byref(size)):
                name = Path(buf.value).stem.lower()
        finally:
            kernel32.CloseHandle(h)
    if len(_proc_names) > 4096:
        _proc_names.clear()
    _proc_names[pid] = name
    return name


def _hwnd_process_name(hwnd) -> str:
    if not hwnd:
        return ""
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    return _process_name(pid.value)


def _foreground_hwnd() -> int:
    return user32.GetForegroundWindow() or 0


def _top_level_windows() -> list[auto.Control]:
    try:
        return auto.GetRootControl().GetChildren()
    except Exception:
        return []


# ---------------------------------------------------------------- nodes


def _safe(fn, default=None):
    try:
        return fn()
    except Exception:
        return default


class UiaNode(Node):
    def __init__(self, ctrl: auto.Control):
        self.ctrl = ctrl

    def _pattern(self, pid: int):
        return _safe(lambda: self.ctrl.GetPattern(pid))

    def hwnd(self) -> int:
        return _safe(lambda: self.ctrl.NativeWindowHandle, 0) or 0

    def role(self) -> str:
        name = _safe(lambda: self.ctrl.ControlTypeName, "") or ""
        return name.removesuffix("Control").lower()

    def name(self) -> str:
        return _safe(lambda: self.ctrl.Name, "") or ""

    def text(self, limit: int = 80) -> str:
        vp = self._pattern(auto.PatternId.ValuePattern)
        if vp is not None:
            value = _safe(lambda: vp.Value, "") or ""
            return value[:limit].replace("\r", " ").replace("\n", " ")
        if self.role() in ("edit", "document"):
            tp = self._pattern(auto.PatternId.TextPattern)
            if tp is not None:
                value = _safe(lambda: tp.DocumentRange.GetText(limit), "") or ""
                return value.replace("\r", " ").replace("\n", " ")
        return ""

    def full_text(self, limit: int) -> str:
        tp = self._pattern(auto.PatternId.TextPattern)
        if tp is not None:
            value = _safe(lambda: tp.DocumentRange.GetText(limit))
            if value is not None:
                return value.replace("\r\n", "\n").replace("\r", "\n")
        vp = self._pattern(auto.PatternId.ValuePattern)
        if vp is not None:
            value = _safe(lambda: vp.Value)
            if value is not None:
                return value[:limit]
        return self.name()

    def _editable(self) -> bool:
        vp = self._pattern(auto.PatternId.ValuePattern)
        if vp is not None:
            return not _safe(lambda: vp.IsReadOnly, True)
        # Rich edit controls (e.g. Windows 11 Notepad) expose TextPattern only.
        return (self.role() in ("edit", "document")
                and bool(_safe(lambda: self.ctrl.IsKeyboardFocusable, False))
                and bool(_safe(lambda: self.ctrl.IsEnabled, False)))

    def states(self) -> list[str]:
        out = []
        if self.is_focused():
            out.append("focused")
        tg = self._pattern(auto.PatternId.TogglePattern)
        if tg is not None and _safe(lambda: tg.ToggleState) == auto.ToggleState.On:
            out.append("checked")
        si = self._pattern(auto.PatternId.SelectionItemPattern)
        if si is not None and _safe(lambda: si.IsSelected, False):
            out.append("selected")
        ec = self._pattern(auto.PatternId.ExpandCollapsePattern)
        if ec is not None and _safe(lambda: ec.ExpandCollapseState) in (
                auto.ExpandCollapseState.Expanded, auto.ExpandCollapseState.PartiallyExpanded):
            out.append("expanded")
        if self.role() == "window":
            hwnd = self.hwnd()
            if hwnd and hwnd == _foreground_hwnd():
                out.append("active")
        if self._editable():
            out.append("editable")
        if not _safe(lambda: self.ctrl.IsEnabled, True):
            out.append("disabled")
        return out

    def _action_table(self) -> list[tuple[str, callable]]:
        table = []
        inv = self._pattern(auto.PatternId.InvokePattern)
        if inv is not None:
            table.append(("invoke", lambda: inv.Invoke(waitTime=0)))
        tg = self._pattern(auto.PatternId.TogglePattern)
        if tg is not None:
            table.append(("toggle", lambda: tg.Toggle(waitTime=0)))
        ec = self._pattern(auto.PatternId.ExpandCollapsePattern)
        if ec is not None:
            table.append(("expand", lambda: ec.Expand(waitTime=0)))
            table.append(("collapse", lambda: ec.Collapse(waitTime=0)))
        si = self._pattern(auto.PatternId.SelectionItemPattern)
        if si is not None:
            table.append(("select", lambda: si.Select(waitTime=0)))
        if inv is None:
            la = self._pattern(auto.PatternId.LegacyIAccessiblePattern)
            if la is not None and _safe(lambda: la.DefaultAction, ""):
                table.append(("default", lambda: la.DoDefaultAction(waitTime=0)))
        return table

    def actions(self) -> list[str]:
        return [name for name, _ in self._action_table()]

    def do_action(self, index: int) -> bool:
        table = self._action_table()
        if not 0 <= index < len(table):
            raise ValueError("action index out of range")
        return bool(_safe(table[index][1], False))

    def set_text(self, text: str) -> bool:
        vp = self._pattern(auto.PatternId.ValuePattern)
        if vp is None or _safe(lambda: vp.IsReadOnly, True):
            raise ValueError("element has no writable value; use focus + type_text")
        return bool(_safe(lambda: vp.SetValue(text, waitTime=0), False))

    def children(self) -> list[Node]:
        return [UiaNode(c) for c in _safe(self.ctrl.GetChildren, []) or []]

    def showing(self) -> bool:
        return not _safe(lambda: self.ctrl.IsOffscreen, True)

    def app_name(self) -> str:
        pid = _safe(lambda: self.ctrl.ProcessId, 0)
        return _process_name(pid) if pid else ""

    def is_focused(self) -> bool:
        return bool(_safe(lambda: self.ctrl.HasKeyboardFocus, False))


# ---------------------------------------------------------------- backend


class WindowsBackend(Backend):
    platform = "windows"
    allow_file = Path(os.environ.get("APPDATA") or Path.home()) / "desktop-mcp" / "allow.txt"
    default_allow = DEFAULT_ALLOW
    boring_roles = {"pane", "group", "custom"}
    apps_note = ("App names are executable names without .exe. Store apps may run "
                 "inside 'applicationframehost'. Windows of elevated (admin) apps "
                 "cannot receive input from a non-elevated server.")

    def enable(self) -> None:
        pass  # UI Automation is always available

    def list_apps(self) -> list[AppInfo]:
        apps: dict[str, AppInfo] = {}
        fg = _foreground_hwnd()
        for w in _top_level_windows():
            node = UiaNode(w)
            name = node.app_name()
            if not name:
                continue
            hwnd = node.hwnd()
            info = apps.setdefault(name, AppInfo(name))
            info.windows.append(WindowInfo(node.role(), node.name().strip(), bool(hwnd) and hwnd == fg))
        return sorted(apps.values(), key=lambda a: a.name)

    def app_root(self, name: str) -> Node | None:
        # UIA has no application object: group top-level windows by executable.
        app = name.lower()

        def windows() -> list[Node]:
            return [n for n in (UiaNode(w) for w in _top_level_windows())
                    if n.app_name() == app]

        root = AppGroupNode(app, windows)
        return root if root.children() else None

    def active_app(self) -> str:
        return _hwnd_process_name(_foreground_hwnd())

    def try_focus(self, node: Node) -> str | None:
        if not isinstance(node, UiaNode):
            raise ValueError("element cannot take focus")
        if _safe(node.ctrl.SetFocus, False):
            time.sleep(0.05)
            if node.is_focused():
                return "SetFocus"
        return None

    def element_center(self, node: Node) -> tuple[int, int]:
        if not isinstance(node, UiaNode):
            raise RuntimeError("element has no on-screen area")
        rect = _safe(lambda: node.ctrl.BoundingRectangle)
        if rect is None or rect.width() <= 0 or rect.height() <= 0:
            raise RuntimeError("element has no on-screen area")
        return rect.xcenter(), rect.ycenter()

    def press_combo(self, combo: str) -> None:
        vks = [_vk(k) for k in combo.replace(" ", "").split("+") if k]
        downs = [_key(vk, 0, _key_flags(vk, False)) for vk in vks]
        ups = [_key(vk, 0, _key_flags(vk, True)) for vk in reversed(vks)]
        _send(downs)
        time.sleep(0.02)
        _send(ups)
        time.sleep(0.03)

    def type_text(self, text: str) -> None:
        # KEYEVENTF_UNICODE sends characters independent of the keyboard layout
        # and leaves the clipboard alone. Newlines and tabs go as real keys.
        text = text.replace("\r\n", "\n").replace("\r", "\n")
        batch: list[INPUT] = []
        for ch in text:
            if ch in "\n\t":
                vk = 0x0D if ch == "\n" else 0x09
                batch += [_key(vk, 0, 0), _key(vk, 0, KEYEVENTF_KEYUP)]
                continue
            data = ch.encode("utf-16-le")
            for i in range(0, len(data), 2):
                unit = int.from_bytes(data[i:i + 2], "little")
                batch += [_key(0, unit, KEYEVENTF_UNICODE),
                          _key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]
            if len(batch) >= 200:
                _send(batch)
                batch = []
        _send(batch)

    def pointer(self, x: int, y: int, button: str = "left", clicks: int = 1,
                scroll: int = 0) -> None:
        if not user32.SetCursorPos(int(x), int(y)):
            raise OSError(ctypes.get_last_error(), "SetCursorPos failed")
        time.sleep(0.05)
        down, up = MOUSE_FLAGS[button]
        for _ in range(max(0, min(clicks, 3))):
            _send([_mouse(down)])
            time.sleep(0.03)
            _send([_mouse(up)])
            time.sleep(0.06)
        if scroll:
            _send([_mouse(MOUSEEVENTF_WHEEL, -scroll * WHEEL_DELTA)])

    def activate_window(self, query: str, allowed: set[str]) -> None:
        q = query.lower()
        for w in _top_level_windows():
            node = UiaNode(w)
            app = node.app_name()
            if app not in allowed:
                continue
            if q in node.name().lower() or q in app:
                hwnd = node.hwnd()
                if not hwnd:
                    continue
                if user32.IsIconic(hwnd):
                    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
                # Windows only lets a process take the foreground right after
                # input; a bare Alt tap satisfies that rule.
                _send([_key(0x12, 0, 0), _key(0x12, 0, KEYEVENTF_KEYUP)])
                user32.SetForegroundWindow(hwnd)
                time.sleep(0.2)
                return

    def screenshot(self) -> str | None:
        from PIL import ImageGrab

        hwnd = _foreground_hwnd()
        rect = wintypes.RECT()
        if not hwnd or not user32.GetWindowRect(hwnd, ctypes.byref(rect)):
            return None
        fd, path = tempfile.mkstemp(prefix="desktop-mcp-", suffix=".png")
        os.close(fd)
        img = ImageGrab.grab(bbox=(rect.left, rect.top, rect.right, rect.bottom),
                             all_screens=True)
        img.save(path)
        return path
