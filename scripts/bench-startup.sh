#!/usr/bin/env bash
# Measures startup time and memory of the built binary.
#
# Time is measured until the application owns its D-Bus name, which happens
# right before the window is mapped. Memory is steady-state PSS after 3 s.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${BIN:-"$ROOT/target/release/gearlever-r"}
RUNS=${RUNS:-3}
APP_ID="io.github.alarez777.GearLeverR"

if [ ! -x "$BIN" ]; then
    echo "error: $BIN not found; run 'cargo build --release -p gearlever' first" >&2
    exit 1
fi

for run in $(seq 1 "$RUNS"); do
    start=$(date +%s%N)
    "$BIN" >/dev/null 2>&1 &
    pid=$!

    for _ in $(seq 1 2000); do
        if dbus-send --session --dest=org.freedesktop.DBus \
            --type=method_call --print-reply / \
            org.freedesktop.DBus.NameHasOwner "string:$APP_ID" 2>/dev/null |
            grep -q "boolean true"; then
            break
        fi
        sleep 0.005
    done

    end=$(date +%s%N)
    startup_ms=$(( (end - start) / 1000000 ))

    sleep 3

    pss=$(awk '/^Pss:/ {print $2}' "/proc/$pid/smaps_rollup" 2>/dev/null || echo "?")
    echo "run $run: startup=${startup_ms}ms pss=${pss}kB"

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    sleep 1
done
