#!/usr/bin/env bash
# Builds the release AppImage inside Debian 12, which keeps it compatible with
# glibc 2.36, GTK 4.8 and libadwaita 1.2.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
RUNTIME=${CONTAINER_RUNTIME:-podman}
IMAGE=${BUILD_IMAGE:-rust:1.98-bookworm}
ARCH=${ARCH:-x86_64}
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

# Fetch the runtime and the DwarFS extractor on the host so the bind-mounted
# cache is not owned by root.
ARCH="$ARCH" "$ROOT/packaging/appimage/fetch-uruntime.sh"
ARCH="$ARCH" "$ROOT/packaging/appimage/fetch-dwarfs.sh"

echo ":: building inside $IMAGE"
"$RUNTIME" run --rm \
    -v "$ROOT:/src:z" \
    -w /src \
    -e CARGO_TARGET_DIR=/src/target-debian12 \
    -e RUSTFLAGS="-C link-arg=-L/usr/lib/x86_64-linux-gnu" \
    -e VARIANTS="${VARIANTS:-all}" \
    -e ARCH="$ARCH" \
    "$IMAGE" bash -c '
        export PATH=/usr/local/cargo/bin:$PATH
        apt-get update -qq >/dev/null
        apt-get install -y -qq pkg-config libgtk-4-dev libadwaita-1-dev \
            libglib2.0-dev libglib2.0-bin liblzma-dev zlib1g-dev xz-utils gettext \
            curl file librsvg2-dev libgirepository1.0-dev \
            libgdk-pixbuf2.0-bin adwaita-icon-theme libgtk-4-bin >/dev/null
        cargo build --release --locked -p gearlever

        echo ":: packaging inside $IMAGE"
        GEARLEVER_R_BINARY=/src/target-debian12/release/gearlever-r \
        APPIMAGETOOL=/src/.cache/appimagetool-x86_64.AppImage \
        APPIMAGE_EXTRACT_AND_RUN=1 \
        VARIANT="$VARIANTS" \
            ./packaging/appimage/build.sh
    '
