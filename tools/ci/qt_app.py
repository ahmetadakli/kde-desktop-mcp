"""The window tools/ci/kde_headless_test.py drives: a line edit, a multi-line
editor, a button, a check box and a status label, all through accessibility.

The status label shows what happened: "Status: hello <name>" after Greet,
"Status: shortcut" after Ctrl+K (which only real key events trigger).
While Tick is checked, the clock label counts every 30 ms (a page that
never stops changing, for wait_idle). A
second window, OTHER, covers it for the background screenshot check. Its
AT-SPI application name and Wayland app id are both "desktop-mcp-ci", the
name the test's allowlist holds.

Needs PyQt6 (or PyQt5) and Qt's Wayland platform plugin.
"""

from __future__ import annotations

import sys

try:
    from PyQt6.QtCore import QTimer
    from PyQt6.QtGui import QKeySequence, QShortcut
    from PyQt6.QtWidgets import (QApplication, QCheckBox, QLabel, QLineEdit, QPlainTextEdit,
                                 QPushButton, QVBoxLayout, QWidget)
except ImportError:  # PyQt5 keeps QShortcut in QtWidgets
    from PyQt5.QtCore import QTimer
    from PyQt5.QtGui import QKeySequence
    from PyQt5.QtWidgets import (QApplication, QCheckBox, QLabel, QLineEdit, QPlainTextEdit,
                                 QPushButton, QShortcut, QVBoxLayout, QWidget)

NAME = "desktop-mcp-ci"
TITLE = "desktop-mcp CI window"
OTHER = "desktop-mcp CI other"


def main() -> int:
    QApplication.setApplicationName(NAME)
    QApplication.setDesktopFileName(NAME)
    app = QApplication(sys.argv)

    win = QWidget()
    win.setWindowTitle(TITLE)
    layout = QVBoxLayout(win)

    name = QLineEdit()
    name.setAccessibleName("Name")
    notes = QPlainTextEdit()
    notes.setAccessibleName("Notes")
    greet = QPushButton("Greet")
    agree = QCheckBox("Agree")
    status = QLabel("Status: ready")
    tick = QCheckBox("Tick")
    clock = QLabel("Clock: 0")

    for w in (name, notes, greet, agree, status, tick, clock):
        layout.addWidget(w)

    greet.clicked.connect(lambda: status.setText(f"Status: hello {name.text()}"))
    agree.toggled.connect(lambda on: status.setText(f"Status: agree {'on' if on else 'off'}"))
    timer = QTimer(win)
    timer.setInterval(30)
    count = [0]

    def advance() -> None:
        count[0] += 1
        clock.setText(f"Clock: {count[0]}")

    timer.timeout.connect(advance)
    tick.toggled.connect(lambda on: timer.start() if on else timer.stop())
    shortcut = QShortcut(QKeySequence("Ctrl+K"), win)
    shortcut.activated.connect(lambda: status.setText("Status: shortcut"))

    win.resize(480, 360)
    win.show()

    other = QLabel("Other window")
    other.setWindowTitle(OTHER)
    other.resize(480, 360)
    other.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
