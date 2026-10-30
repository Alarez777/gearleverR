#!/usr/bin/env bash
# Builds the release AppImage inside Debian 12, which keeps it compatible with
# glibc 2.36, GTK 4.8 and libadwaita 1.2.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
RUNTIME=${CONTAINER_RUNTIME:-podman}
IMAGE=${BUILD_IMAGE:-rust:1.98-bookworm}
TOOL=${APPIMAGETOOL:-$ROOT/.cache/appimagetool-x86_64.AppImage}

if ! command -v "$RUNTIME" >/dev/null 2>&1; then
    echo "error: $RUNTIME is required (install podman or docker)" >&2
    exit 1
fi

if [ ! -x "$TOOL" ]; then
    echo ":: downloading appimagetool"
    mkdir -p "$(dirname "$TOOL")"
    curl -fsSL -o "$TOOL" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$TOOL"
fi

echo ":: building inside $IMAGE"
"$RUNTIME" run --rm \
    -v "$ROOT:/src:z" \
    -w /src \
    -e CARGO_TARGET_DIR=/src/target-debian12 \
    -e RUSTFLAGS="-C link-arg=-L/usr/lib/x86_64-linux-gnu" \
    "$IMAGE" bash -c '
        export PATH=/usr/local/cargo/bin:$PATH
        apt-get update -qq >/dev/null
        apt-get install -y -qq pkg-config libgtk-4-dev libadwaita-1-dev \
            libglib2.0-dev liblzma-dev zlib1g-dev >/dev/null
        cargo build --release --locked -p gearlever
    '

echo ":: packaging"
GEARLEVER_R_BINARY="$ROOT/target-debian12/release/gearlever-r" \
APPIMAGETOOL="$TOOL" \
APPIMAGE_EXTRACT_AND_RUN=1 \
    "$ROOT/packaging/appimage/build.sh"
