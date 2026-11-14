#!/usr/bin/env bash
# Fetches and verifies the pinned uruntime used by the AppImage builds.
#
# The runtime is pinned by release and SHA-256. The extraction policy is set to
# 2 (extract on FUSE failure regardless of image size) so the AppImage keeps
# starting on systems without FUSE. The patch only replaces one ASCII digit, so
# the runtime size and the payload offset do not change.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ARCH=${ARCH:-x86_64}
OUT=${1:-$ROOT/.cache/uruntime-appimage-squashfs-lite-$ARCH}

URUNTIME_VERSION=v0.8.2
URUNTIME_NAME=uruntime-appimage-squashfs-lite-$ARCH
URUNTIME_URL="https://github.com/VHSgunzo/uruntime/releases/download/$URUNTIME_VERSION/$URUNTIME_NAME"

case "$ARCH" in
    x86_64)
        SHA256_ORIGINAL=ba8aedc25c4b4f924636b3b81f8c7ee6ce4132499313ead9417d002a2a401693
        SHA256_PATCHED=f74c77ec07e05fc1c20005a01cd7206bad62e1c893e297864667fae79145de2f
        ;;
    *)
        echo "error: no pinned uruntime for architecture '$ARCH'" >&2
        exit 1
        ;;
esac

verify() {
    local file=$1
    local sum=$2
    echo "$sum  $file" | sha256sum -c -
}

# Verify the cached runtime (already patched) before touching the network.
if [ -f "$OUT" ] && verify "$OUT" "$SHA256_PATCHED"; then
    chmod 755 "$OUT"
    exit 0
fi

mkdir -p "$(dirname "$OUT")"
tmp=$(mktemp "$OUT.XXXXXX")
trap 'rm -f "$tmp"' EXIT

echo ":: downloading uruntime $URUNTIME_VERSION ($ARCH)"
curl -fsSL -o "$tmp" "$URUNTIME_URL"
verify "$tmp" "$SHA256_ORIGINAL"

sed -i 's|URUNTIME_EXTRACT=[0-9]|URUNTIME_EXTRACT=2|' "$tmp"
if ! grep -a -q 'URUNTIME_EXTRACT=2' "$tmp"; then
    echo "error: could not set URUNTIME_EXTRACT=2 in $URUNTIME_NAME" >&2
    exit 1
fi
verify "$tmp" "$SHA256_PATCHED"

chmod 755 "$tmp"
mv -f "$tmp" "$OUT"
trap - EXIT
