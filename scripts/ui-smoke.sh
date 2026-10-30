#!/usr/bin/env bash
# Startup smoke test with real pointer input:
#   - the main window appears without a blocking modal
#   - a click on the + button opens the file chooser
#
# Needs dbus-run-session, xvfb-run, openbox, xdotool and xwininfo
# (Debian: dbus-x11, xvfb, openbox, xdotool, x11-utils). A window manager is
# required because synthetic X clicks are not delivered reliably without one.
set -euo pipefail

BIN=${1:?usage: ui-smoke.sh <path-to-gearlever-r-or-AppImage>}
export BIN
export APPIMAGE_EXTRACT_AND_RUN=1
export LANG=C.UTF-8
export LANGUAGE=C

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/home"
export HOME="$WORK/home"

dbus-run-session -- xvfb-run -a -s "-screen 0 1000x900x24" bash -c '
    set -euo pipefail
    openbox >/dev/null 2>&1 &
    sleep 1

    "$BIN" >"$HOME/gui.log" 2>&1 &
    pid=$!
    sleep 6

    if ! xwininfo -root -tree 2>/dev/null | grep "Gear Lever R" >/dev/null; then
        echo "main window did not appear" >&2
        cat "$HOME/gui.log" >&2
        kill $pid 2>/dev/null
        exit 1
    fi

    if xwininfo -root -tree 2>/dev/null | grep "Welcome" >/dev/null; then
        echo "a modal welcome window is blocking the main window" >&2
        kill $pid 2>/dev/null
        exit 1
    fi

    id=$(xdotool search --name "Gear Lever R" | head -1)
    xdotool windowactivate --sync "$id" 2>/dev/null || true
    sleep 1
    xdotool mousemove --window "$id" 30 28
    sleep 0.2
    xdotool click 1

    for _ in $(seq 1 30); do
        xwininfo -root -tree 2>/dev/null | grep "Open an AppImage" >/dev/null && break
        sleep 0.2
    done

    if ! xwininfo -root -tree 2>/dev/null | grep "Open an AppImage" >/dev/null; then
        echo "the + button did not react to a real click" >&2
        xwininfo -root -tree 2>/dev/null | grep -E "Gear|Open|Welcome" >&2
        cat "$HOME/gui.log" >&2
        kill $pid 2>/dev/null
        exit 1
    fi

    echo "OK: no modal and the + button opens the chooser with a real click"
'
