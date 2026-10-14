"""Platform-neutral contract between the MCP tool layer and a desktop backend.

The tool layer (server.py) owns ids, the allowlist, tree rendering and the order
of fallbacks. A backend only knows how to read and drive its platform's
accessibility tree and input devices.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from pathlib import Path


class Node(ABC):
    """One element of an accessibility tree."""

    @abstractmethod
    def role(self) -> str: ...

    @abstractmethod
    def name(self) -> str: ...

    @abstractmethod
    def text(self, limit: int = 80) -> str:
        """Short single-line text preview, "" when the element has no text."""

    @abstractmethod
    def full_text(self, limit: int) -> str:
        """Complete text content (falls back to the name)."""

    @abstractmethod
    def states(self) -> list[str]:
        """Subset of: focused, checked, selected, expanded, active, editable, disabled."""

    @abstractmethod
    def actions(self) -> list[str]: ...

    @abstractmethod
    def do_action(self, index: int) -> bool: ...

    @abstractmethod
    def set_text(self, text: str) -> bool:
        """Replace the contents; raise ValueError when the element is not editable."""

    @abstractmethod
    def children(self) -> list["Node"]: ...

    @abstractmethod
    def showing(self) -> bool: ...

    @abstractmethod
    def app_name(self) -> str:
        """Name used for allowlist checks."""

    @abstractmethod
    def is_focused(self) -> bool: ...


@dataclass
class WindowInfo:
    kind: str
    title: str
    active: bool


@dataclass
class AppInfo:
    name: str
    windows: list[WindowInfo] = field(default_factory=list)


class Backend(ABC):
    platform: str
    allow_file: Path
    default_allow: str
    # Roles that are skipped (their children are lifted) when unnamed and actionless.
    boring_roles: set[str]
    apps_note: str = ""

    @abstractmethod
    def enable(self) -> None:
        """Make sure the accessibility layer is on. Called before reads."""

    @abstractmethod
    def list_apps(self) -> list[AppInfo]: ...

    @abstractmethod
    def app_root(self, name: str) -> Node | None:
        """Root node whose children are the app's top-level windows."""

    @abstractmethod
    def active_app(self) -> str:
        """App name owning the foreground window, "" when unknown."""

    @abstractmethod
    def try_focus(self, node: Node) -> str | None:
        """Focus without synthesising pointer input. Return the method that
        worked, or None so the caller can fall back to a click."""

    @abstractmethod
    def element_center(self, node: Node) -> tuple[int, int]:
        """Centre of the element in the coordinate space `pointer` accepts."""

    @abstractmethod
    def press_combo(self, combo: str) -> None:
        """Press and release a chord such as "ctrl+s"."""

    @abstractmethod
    def type_text(self, text: str) -> None: ...

    @abstractmethod
    def pointer(self, x: int, y: int, button: str = "left", clicks: int = 1,
                scroll: int = 0) -> None: ...

    @abstractmethod
    def activate_window(self, query: str, allowed: set[str]) -> None:
        """Raise the first window of an allowed app whose title or app name
        contains `query`."""

    @abstractmethod
    def screenshot(self) -> str | None:
        """Capture the active window to a private file and return its path."""
