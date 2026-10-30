#!/usr/bin/env bash
# Times metadata extraction on real AppImages, with no subprocesses involved.
#
# Usage: scripts/bench-metadata.sh ~/AppImages/*.appimage
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <file.AppImage>..." >&2
    exit 1
fi

cargo build --release --quiet -p gearlever-core --example inspect

for file in "$@"; do
    printf '%-45s ' "$(basename "$file")"
    "$ROOT/target/release/examples/inspect" "$file" |
        awk '/^elapsed:/ {print $2; found=1} END {if (!found) print "failed"}'
done
