#!/usr/bin/env bash
# Assembles the thin AppImage (GTK4/libadwaita provided by the system).
#
# Usage:
#   packaging/appimage/build.sh
#   APPIMAGETOOL=/path/to/appimagetool packaging/appimage/build.sh
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ARCH=${ARCH:-x86_64}
APPDIR="$ROOT/packaging/appimage/AppDir"
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)
OUTPUT="$ROOT/dist/GearLeverR-${VERSION}-${ARCH}.AppImage"

APPIMAGETOOL=${APPIMAGETOOL:-appimagetool}
if ! command -v "$APPIMAGETOOL" >/dev/null 2>&1 && [ ! -x "$APPIMAGETOOL" ]; then
    echo "error: appimagetool not found; set APPIMAGETOOL=/path/to/appimagetool" >&2
    exit 1
fi

if [ -n "${GEARLEVER_R_BINARY:-}" ]; then
    BINARY="$GEARLEVER_R_BINARY"
    echo ":: using prebuilt binary $BINARY"
else
    echo ":: building gearlever-r ${VERSION} for ${ARCH}"
    cargo build --release --locked -p gearlever --manifest-path "$ROOT/Cargo.toml"
    BINARY="$ROOT/target/release/gearlever-r"
fi

rm -rf "$APPDIR"
install -Dm755 "$BINARY" "$APPDIR/gearlever-r"
install -Dm755 "$ROOT/packaging/appimage/AppRun" "$APPDIR/AppRun"
install -Dm644 "$ROOT/data/io.github.alarez777.GearLeverR.desktop" \
    "$APPDIR/io.github.alarez777.GearLeverR.desktop"
install -Dm644 "$ROOT/data/icons/io.github.alarez777.GearLeverR.svg" \
    "$APPDIR/io.github.alarez777.GearLeverR.svg"
install -Dm644 "$ROOT/data/icons/io.github.alarez777.GearLeverR.png" \
    "$APPDIR/io.github.alarez777.GearLeverR.png"
ln -sf io.github.alarez777.GearLeverR.png "$APPDIR/.DirIcon"
install -Dm644 "$ROOT/data/icons/io.github.alarez777.GearLeverR.svg" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/io.github.alarez777.GearLeverR.svg"
install -Dm644 "$ROOT/data/icons/io.github.alarez777.GearLeverR.png" \
    "$APPDIR/usr/share/icons/hicolor/256x256/apps/io.github.alarez777.GearLeverR.png"
install -Dm644 "$ROOT/data/io.github.alarez777.GearLeverR.gschema.xml" \
    "$APPDIR/usr/share/glib-2.0/schemas/io.github.alarez777.GearLeverR.gschema.xml"

if command -v glib-compile-schemas >/dev/null 2>&1; then
    glib-compile-schemas "$APPDIR/usr/share/glib-2.0/schemas"
else
    echo "warning: glib-compile-schemas not found; settings will fall back to defaults" >&2
fi

mkdir -p "$ROOT/dist"
echo ":: packaging $OUTPUT"
ARCH="$ARCH" "$APPIMAGETOOL" "$APPDIR" "$OUTPUT"

echo ":: done: $OUTPUT"
