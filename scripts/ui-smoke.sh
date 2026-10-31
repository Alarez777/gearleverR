#!/usr/bin/env bash
# Startup smoke test with real pointer input:
#   - a pristine home shows the first-run window
#   - once Gear LeverR is integrated, the main window appears without a modal
#   - a click on the + button opens the file chooser
#
# Needs dbus-run-session, xvfb-run, openbox, xdotool and xwininfo
# (Debian: dbus-x11, xvfb, openbox, xdotool, x11-utils). A window manager is
# required because synthetic X clicks are not delivered reliably without one.
set -euo pipefail

BIN=${1:?usage: ui-smoke.sh <path-to-gearlever-r-or-AppImage>}
BIN=$(readlink -f "$BIN")
export BIN
export APPIMAGE_EXTRACT_AND_RUN=1
export LANG=C.UTF-8
export LANGUAGE=C
# The test drives the X11 pointer, so do not let a Wayland session win.
export GDK_BACKEND=x11
unset WAYLAND_DISPLAY
export NO_AT_BRIDGE=1

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/home"
export HOME="$WORK/home"

# A live GNOME session can make GtkApplication wait on its portals inside a
# fresh session bus; set SMOKE_DBUS=0 to drive the X11 display directly.
launcher=()
if [ "${SMOKE_DBUS:-1}" = "1" ]; then
    launcher=(dbus-run-session --)
fi

"${launcher[@]}" xvfb-run -a -s "-screen 0 1000x900x24" bash -c '
    set -euo pipefail
    openbox >/dev/null 2>&1 &
    sleep 1

    # First launch: no desktop entry yet, so the first-run window must appear.
    "$BIN" >"$HOME/first-run.log" 2>&1 &
    pid=$!
    sleep 6

    if ! xwininfo -root -tree 2>/dev/null | grep "Welcome to Gear LeverR" >/dev/null; then
        echo "the first-run window did not appear" >&2
        xwininfo -root -tree 2>/dev/null | grep -E "Gear|Welcome" >&2
        cat "$HOME/first-run.log" >&2
        kill $pid 2>/dev/null
        exit 1
    fi

    # APPIMAGE_EXTRACT_AND_RUN spawns the real binary as a child.
    pkill -P $pid 2>/dev/null || true
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
    for _ in $(seq 1 20); do
        xwininfo -root -tree 2>/dev/null | grep "Welcome to Gear LeverR" >/dev/null || break
        sleep 0.2
    done

    # Pretend a previous run already integrated Gear LeverR: the main window
    # must open directly and react to a real click.
    mkdir -p "$HOME/.local/share/applications"
    printf "[Desktop Entry]\nName=Gear LeverR\nTryExec=%s\nExec=%s %%U\n" \
        "$BIN" "$BIN" >"$HOME/.local/share/applications/io.github.alarez777.GearLeverR.desktop"

    "$BIN" >"$HOME/gui.log" 2>&1 &
    pid=$!
    sleep 6

    if ! xwininfo -root -tree 2>/dev/null | grep "\"Gear LeverR\"" >/dev/null; then
        echo "main window did not appear" >&2
        cat "$HOME/gui.log" >&2
        kill $pid 2>/dev/null
        exit 1
    fi

    if xwininfo -root -tree 2>/dev/null | grep "Welcome to Gear LeverR" >/dev/null; then
        echo "the first-run window appeared even though Gear LeverR is integrated" >&2
        kill $pid 2>/dev/null
        exit 1
    fi

    id=$(xdotool search --name "^Gear LeverR$" | head -1)
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

    echo "OK: first-run window shown once, then the + button opens the chooser"
'
