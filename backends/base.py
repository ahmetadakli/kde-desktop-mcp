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


class AppGroupNode(Node):
    """Synthetic root holding every top-level window of one app name, even when
    several processes share that name."""

    def __init__(self, app: str, windows):
        self.app = app
        self._windows = windows  # callable returning list[Node]

    def role(self) -> str:
        return "application"

    def name(self) -> str:
        return self.app

    def text(self, limit: int = 80) -> str:
        return ""

    def full_text(self, limit: int) -> str:
        return self.app

    def states(self) -> list[str]:
        return []

    def actions(self) -> list[str]:
        return []

    def do_action(self, index: int) -> bool:
        raise ValueError("application root has no actions")

    def set_text(self, text: str) -> bool:
        raise ValueError("application root is not editable")

    def children(self) -> list[Node]:
        return self._windows()

    def showing(self) -> bool:
        return True

    def app_name(self) -> str:
        return self.app

    def is_focused(self) -> bool:
        return False


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

    def warm_up(self) -> None:
        """Prepare slow resources (virtual input devices) ahead of first use.
        Runs on a background thread at startup."""

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
