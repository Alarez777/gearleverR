#!/usr/bin/env bash
# Compiles po/*.po into <output>/<lang>/LC_MESSAGES/gearlever-r.mo
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUT=${1:-"$ROOT/target/locale"}

if ! command -v msgfmt >/dev/null 2>&1; then
    echo "error: msgfmt is required (install the gettext package)" >&2
    exit 1
fi

for po in "$ROOT"/po/*.po; do
    lang=$(basename "$po" .po)
    directory="$OUT/$lang/LC_MESSAGES"
    mkdir -p "$directory"
    msgfmt --check --output-file "$directory/gearlever-r.mo" "$po"
done

echo "translations compiled into $OUT"
