#!/usr/bin/env bash
# Headless KDE integration test, for CI only: the "KDE (headless)" job runs
# it in an Arch Linux container (KWin 6) with the server built outside:
#
#   docker run ... archlinux:latest bash /src/tools/ci/kde_headless.sh \
#       /src/rust/target/release/desktop-mcp [--input auto|require|skip]
#
# As root it installs KWin, AT-SPI and PyQt6, then reruns itself as an
# unprivileged user inside a new D-Bus session (dbus-run-session). There it
# starts kwin_wayland --virtual on a private XDG_RUNTIME_DIR, turns on
# accessibility and runs kde_headless_test.py. Logs (KWin, the app, the
# server's stderr) go to $CI_LOG_DIR.
#
# It refuses to run outside a GitHub Actions container: starting a session
# bus and a compositor next to a running desktop can break that desktop's
# accessibility bus.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
logs=${CI_LOG_DIR:-/logs}
socket=desktop-mcp-ci

in_ci() {
    [ "${GITHUB_ACTIONS:-}" = true ] && [ -f /.dockerenv ] || {
        echo "refusing to run: only for the CI container (see the comment at the top)" >&2
        exit 2
    }
}

container() {
    in_ci
    [ "$(id -u)" = 0 ] || { echo "the container stage runs as root" >&2; exit 2; }
    pacman -Syu --noconfirm --needed kwin at-spi2-core python-pyqt6 qt6-wayland \
        dbus glib2 mesa ttf-dejavu >/dev/null
    pacman -Q kwin at-spi2-core python-pyqt6
    id ci >/dev/null 2>&1 || useradd -m ci
    if [ -e /dev/uinput ]; then
        chmod 0666 /dev/uinput
    else
        echo "no /dev/uinput in the container: input tests will be skipped"
    fi
    mkdir -p "$logs"
    chown ci "$logs"
    exec runuser -u ci -- env HOME=/home/ci USER=ci LANG=C.UTF-8 PYTHONIOENCODING=utf-8 \
        GITHUB_ACTIONS=true CI_LOG_DIR="$logs" \
        dbus-run-session -- bash "$here/kde_headless.sh" --session "$@"
}

session() {
    in_ci
    [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || { echo "no session bus" >&2; exit 2; }
    XDG_RUNTIME_DIR=$(mktemp -d)
    chmod 0700 "$XDG_RUNTIME_DIR"
    export XDG_RUNTIME_DIR XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=KDE \
        QT_QPA_PLATFORM=wayland QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 QT_ACCESSIBILITY=1 \
        KWIN_COMPOSE=Q
    unset DISPLAY WAYLAND_DISPLAY

    # Arch ships kwin_wayland with file capabilities (cap_sys_nice); a
    # container without that capability refuses to exec it ("Operation not
    # permitted"). A plain copy has none.
    kwin_bin=$(mktemp)
    cp "$(command -v kwin_wayland)" "$kwin_bin"
    chmod 0755 "$kwin_bin"
    "$kwin_bin" --virtual --width 1280 --height 800 --socket "$socket" \
        --no-lockscreen --no-global-shortcuts >"$logs/kwin.log" 2>&1 &
    kwin=$!
    trap 'kill "$kwin" 2>/dev/null || true' EXIT
    for _ in $(seq 100); do
        [ -S "$XDG_RUNTIME_DIR/$socket" ] && break
        kill -0 "$kwin" 2>/dev/null || { echo "KWin exited:"; cat "$logs/kwin.log"; exit 1; }
        sleep 0.3
    done
    [ -S "$XDG_RUNTIME_DIR/$socket" ] || { echo "no KWin socket:"; cat "$logs/kwin.log"; exit 1; }
    export WAYLAND_DISPLAY=$socket
    # KWin registers its D-Bus name after the socket appears.
    for _ in $(seq 50); do
        gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
            --method org.freedesktop.DBus.NameHasOwner org.kde.KWin | grep -q true && break
        sleep 0.2
    done

    # Starts the accessibility bus (D-Bus activation) and turns it on.
    gdbus call --session --dest org.a11y.Bus --object-path /org/a11y/bus \
        --method org.a11y.Bus.GetAddress
    gdbus call --session --dest org.a11y.Bus --object-path /org/a11y/bus \
        --method org.freedesktop.DBus.Properties.Set org.a11y.Status IsEnabled '<true>'

    DESKTOP_MCP_HEADLESS=1 python3 "$here/kde_headless_test.py" --logs "$logs" "$@"
}

if [ "${1:-}" = --session ]; then
    shift
    session "$@"
else
    container "$@"
fi
