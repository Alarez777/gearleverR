#!/usr/bin/env bash
# Builds the release AppImage inside Debian 12, which keeps it compatible with
# glibc 2.36, GTK 4.8 and libadwaita 1.2.
#
# The container runs natively, so arm64 images must be built on an arm64 host.
# The release workflow does that on the ubuntu-24.04-arm runner.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
RUNTIME=${CONTAINER_RUNTIME:-podman}
IMAGE=${BUILD_IMAGE:-rust:1.98-bookworm}
ARCH=${ARCH:-$(uname -m)}
HOST_ARCH=$(uname -m)
MULTIARCH="${ARCH}-linux-gnu"
TOOL=${APPIMAGETOOL:-$ROOT/.cache/appimagetool-$ARCH.AppImage}

case "$HOST_ARCH" in
    x86_64) PLATFORM=linux/amd64 ;;
    aarch64) PLATFORM=linux/arm64 ;;
    *)
        echo "error: unsupported host architecture '$HOST_ARCH'" >&2
        exit 1
        ;;
esac

if [ "$ARCH" != "$HOST_ARCH" ]; then
    echo "error: building $ARCH AppImages requires an $ARCH host" >&2
    echo "use an $ARCH machine or the release workflow's $ARCH runner" >&2
    exit 1
fi

if ! command -v "$RUNTIME" >/dev/null 2>&1; then
    echo "error: $RUNTIME is required (install podman or docker)" >&2
    exit 1
fi

# A local tag may point at another architecture after testing both images.
# Refresh it explicitly so --platform selects the host variant.
image_arch=$("$RUNTIME" image inspect --format '{{.Architecture}}' "$IMAGE" 2>/dev/null || true)
if [ -n "$image_arch" ] && [ "$image_arch" != "${PLATFORM#linux/}" ]; then
    echo ":: refreshing $IMAGE for $PLATFORM"
    "$RUNTIME" pull --platform "$PLATFORM" "$IMAGE"
fi

case "$TOOL" in
    "$ROOT/.cache/"*) ;;
    *)
        echo "error: APPIMAGETOOL must live under $ROOT/.cache" >&2
        exit 1
        ;;
esac

if [ ! -x "$TOOL" ]; then
    echo ":: downloading appimagetool for $ARCH"
    mkdir -p "$(dirname "$TOOL")"
    curl -fsSL -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
    chmod +x "$TOOL"
fi

# Fetch the runtime and the DwarFS extractor on the host so the bind-mounted
# cache is not owned by root.
ARCH="$ARCH" "$ROOT/packaging/appimage/fetch-uruntime.sh"
ARCH="$ARCH" "$ROOT/packaging/appimage/fetch-dwarfs.sh"

echo ":: building $ARCH inside $IMAGE"
"$RUNTIME" run --rm \
    --platform "$PLATFORM" \
    -v "$ROOT:/src:z" \
    -w /src \
    -e CARGO_TARGET_DIR="/src/target-debian12-$ARCH" \
    -e RUSTFLAGS="-C link-arg=-L/usr/lib/$MULTIARCH" \
    -e VARIANTS="${VARIANTS:-all}" \
    -e ARCH="$ARCH" \
    -e IMAGE="$IMAGE" \
    -e APPIMAGETOOL="/src/.cache/$(basename "$TOOL")" \
    -e BINARY="/src/target-debian12-$ARCH/release/gearlever-r" \
    "$IMAGE" bash -c '
        set -euo pipefail
        export PATH=/usr/local/cargo/bin:$PATH
        apt-get update -qq >/dev/null
        apt-get install -y -qq pkg-config libgtk-4-dev libadwaita-1-dev \
            libglib2.0-dev libglib2.0-bin liblzma-dev zlib1g-dev xz-utils gettext \
            curl file librsvg2-dev libgirepository1.0-dev \
            libgdk-pixbuf2.0-bin adwaita-icon-theme libgtk-4-bin >/dev/null
        cargo build --release --locked -p gearlever

        echo ":: packaging inside $IMAGE"
        GEARLEVER_R_BINARY="$BINARY" \
        APPIMAGETOOL="$APPIMAGETOOL" \
        APPIMAGE_EXTRACT_AND_RUN=1 \
        ARCH="$ARCH" \
        VARIANT="$VARIANTS" \
            ./packaging/appimage/build.sh
    '
