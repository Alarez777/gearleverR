#!/usr/bin/env bash
# Regenerates the POT and rebuilds every catalog from the upstream Gear Lever
# translations plus the overrides maintained in this repository.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
POT="$ROOT/po/gearlever-r.pot"

python3 "$ROOT/scripts/extract-strings.py"

for source in "$ROOT"/po/upstream/*.po; do
    language=$(basename "$source")
    merged="$ROOT/po/$language.merged"
    result="$ROOT/po/$language"

    msgmerge --quiet --no-fuzzy-matching "$source" "$POT" -o "$merged"

    if [ -f "$ROOT/po/overrides/$language" ]; then
        msgcat --use-first "$ROOT/po/overrides/$language" "$merged" -o "$result"
        rm -f "$merged"
    else
        mv "$merged" "$result"
    fi

    msgattrib --no-obsolete --no-fuzzy "$result" -o "$result.clean"
    mv "$result.clean" "$result"
    echo "updated $language"
done
