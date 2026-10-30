#!/usr/bin/env bash
# Startup smoke test: no modal window blocks the interface and the Relm4
# message loop is alive.
#
# Needs dbus-run-session, xvfb-run, xwininfo and gdbus
# (Debian: dbus-x11, xvfb, x11-utils, libglib2.0-bin).
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

fail() { echo "FAIL: $1" >&2; exit 1; }

dbus-run-session -- xvfb-run -a -s "-screen 0 1000x900x24" bash -c '
    set -euo pipefail
    "$BIN" >"$HOME/gui.log" 2>&1 &
    pid=$!

    for _ in $(seq 1 60); do
        xwininfo -root -tree 2>/dev/null | grep -q "Gear Lever R" && break
        sleep 0.2
    done
    xwininfo -root -tree 2>/dev/null | grep -q "Gear Lever R" || {
        echo "main window did not appear" >&2
        cat "$HOME/gui.log" >&2
        kill $pid 2>/dev/null
        exit 1
    }

    if xwininfo -root -tree 2>/dev/null | grep -q "Welcome"; then
        echo "a modal welcome window is blocking the main window" >&2
        kill $pid 2>/dev/null
        exit 1
    fi

    gdbus call --session \
        --dest io.github.alarez777.GearLeverR \
        --object-path /io/github/alarez777/GearLeverR/window/1 \
        --method org.gtk.Actions.Activate preferences "[]" "{}" >/dev/null

    for _ in $(seq 1 30); do
        xwininfo -root -tree 2>/dev/null | grep -q "Preferences" && break
        sleep 0.2
    done

    if xwininfo -root -tree 2>/dev/null | grep -q "Preferences"; then
        echo "OK: no blocking modal, message loop responds"
        status=0
    else
        echo "the preferences action did not open a window" >&2
        xwininfo -root -tree 2>/dev/null | head -20 >&2
        status=1
    fi

    kill $pid 2>/dev/null
    exit $status
'
