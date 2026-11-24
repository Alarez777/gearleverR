#!/usr/bin/env bash
# Fetches and verifies the pinned uruntime used by the AppImage builds.
#
# The runtime is pinned by release and SHA-256. The extraction policy is set to
# 2 (extract on FUSE failure regardless of image size) so the AppImage keeps
# starting on systems without FUSE. The patch only replaces one ASCII digit, so
# the runtime size and the payload offset do not change.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ARCH=${ARCH:-$(uname -m)}
OUT=${1:-$ROOT/.cache/uruntime-appimage-squashfs-lite-$ARCH}

URUNTIME_VERSION=v0.8.5
URUNTIME_NAME=uruntime-appimage-squashfs-lite-$ARCH
URUNTIME_URL="https://github.com/VHSgunzo/uruntime/releases/download/$URUNTIME_VERSION/$URUNTIME_NAME"

# AGENTS.md documents the pinned version for agents and contributors. Keep it
# in sync so a bump cannot leave the guide pointing at an older runtime.
if ! grep -Fq '`uruntime` '"$URUNTIME_VERSION" "$ROOT/AGENTS.md"; then
    echo "error: AGENTS.md does not document uruntime $URUNTIME_VERSION" >&2
    echo "update the AppImage runtime line in AGENTS.md" >&2
    exit 1
fi

case "$ARCH" in
    x86_64)
        SHA256_ORIGINAL=95fcae18298788ee08e8fc29c4e33ac85dcd2b01fb631dda33d60b8ab8e5b4a5
        SHA256_PATCHED=19d89e8ccaa512cbcf21c5accc787cb4d00b13e7bd3e5bd393ced71d4f18c637
        ;;
    aarch64)
        SHA256_ORIGINAL=48dbb5d7ff4bdfdbfbfaf675e6ab21fa6646df693915c066a7443576967045e1
        SHA256_PATCHED=80ff2c16317bcd8af884a5110d136f9ae2ad9c215b6879f679e5da5ef33222b2
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
