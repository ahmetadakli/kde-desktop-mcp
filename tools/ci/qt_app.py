"""The window tools/ci/kde_headless_test.py drives: a line edit, a multi-line
editor, a button, a check box and a status label, all through accessibility.

The status label shows what happened: "Status: hello <name>" after Greet,
"Status: shortcut" after Ctrl+K (which only real key events trigger). Its
AT-SPI application name and Wayland app id are both "desktop-mcp-ci", the
name the test's allowlist holds.

Needs PyQt6 (or PyQt5) and Qt's Wayland platform plugin.
"""

from __future__ import annotations

import sys

try:
    from PyQt6.QtGui import QKeySequence, QShortcut
    from PyQt6.QtWidgets import (QApplication, QCheckBox, QLabel, QLineEdit, QPlainTextEdit,
                                 QPushButton, QVBoxLayout, QWidget)
except ImportError:  # PyQt5 keeps QShortcut in QtWidgets
    from PyQt5.QtGui import QKeySequence
    from PyQt5.QtWidgets import (QApplication, QCheckBox, QLabel, QLineEdit, QPlainTextEdit,
                                 QPushButton, QShortcut, QVBoxLayout, QWidget)

NAME = "desktop-mcp-ci"
TITLE = "desktop-mcp CI window"


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

    for w in (name, notes, greet, agree, status):
        layout.addWidget(w)

    greet.clicked.connect(lambda: status.setText(f"Status: hello {name.text()}"))
    agree.toggled.connect(lambda on: status.setText(f"Status: agree {'on' if on else 'off'}"))
    shortcut = QShortcut(QKeySequence("Ctrl+K"), win)
    shortcut.activated.connect(lambda: status.setText("Status: shortcut"))

    win.resize(480, 360)
    win.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
