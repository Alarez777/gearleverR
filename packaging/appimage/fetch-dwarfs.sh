#!/usr/bin/env bash
# Fetches and verifies the pinned dwarfsextract used to read DwarFS metadata.
#
# The binary is static, so it runs inside the Debian 12 build and on every
# distribution the AppImage supports. Only the read-only extraction tool is
# bundled: dwarfsextract uses the MIT-licensed DwarFS reader libraries.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ARCH=${ARCH:-$(uname -m)}
OUT=${1:-$ROOT/.cache/dwarfsextract-$ARCH}

DWARFS_VERSION=0.15.8
DWARFS_TAG=v$DWARFS_VERSION
TARBALL=dwarfs-$DWARFS_VERSION-Linux-$ARCH.tar.xz
TARBALL_PATH=dwarfs-$DWARFS_VERSION-Linux-$ARCH/bin/dwarfsextract
URL="https://github.com/mhx/dwarfs/releases/download/$DWARFS_TAG/$TARBALL"

case "$ARCH" in
    x86_64)
        SHA256_TARBALL=f5eb34c3e2ac1bfcbe66e15b8a66da20897049b22867d2c193bffedf25cc08ae
        SHA256_BINARY=61d42261baaf2cf9b825b44ff976b7b5d55a746551c32e88d7b5c3d4dfa21709
        ;;
    aarch64)
        SHA256_TARBALL=ee100574a1340ad054bdf7543bedde3bd040639114ede65f1c20e08567eff2b1
        SHA256_BINARY=02041cb70cc75c5d674acbd5492f38d8293d628e6de1ea1f69a395954d450156
        ;;
    *)
        echo "error: no pinned dwarfsextract for architecture '$ARCH'" >&2
        exit 1
        ;;
esac

verify() {
    local file=$1
    local sum=$2
    echo "$sum  $file" | sha256sum -c -
}

# Verify the cached binary before touching the network.
if [ -f "$OUT" ] && verify "$OUT" "$SHA256_BINARY"; then
    chmod 755 "$OUT"
    exit 0
fi

mkdir -p "$(dirname "$OUT")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo ":: downloading dwarfsextract $DWARFS_VERSION ($ARCH)"
curl -fsSL -o "$tmp/$TARBALL" "$URL"
verify "$tmp/$TARBALL" "$SHA256_TARBALL"

tar -xJf "$tmp/$TARBALL" -C "$tmp" --strip-components=2 "$TARBALL_PATH"
verify "$tmp/dwarfsextract" "$SHA256_BINARY"

install -Dm755 "$tmp/dwarfsextract" "$OUT"
trap - EXIT
