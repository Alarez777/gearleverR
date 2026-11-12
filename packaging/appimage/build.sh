#!/usr/bin/env bash
# Assembles the Gear LeverR AppImages.
#
# VARIANT=default  GTK 4 and libadwaita are provided by the host (recommended).
# VARIANT=system   Alias for default.
# VARIANT=bundled  GTK 4, libadwaita and their dependencies are inside.
# VARIANT=all      Build both variants (default).
#
# Usage:
#   packaging/appimage/build.sh
#   VARIANT=default packaging/appimage/build.sh
#   APPIMAGETOOL=/path/to/appimagetool packaging/appimage/build.sh
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ARCH=${ARCH:-x86_64}
VARIANT=${VARIANT:-all}
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)
DIST="$ROOT/dist"
SYSTEM_APPDIR="$ROOT/packaging/appimage/AppDir"
BUNDLED_APPDIR="$ROOT/packaging/appimage/AppDir-bundled"
SYSTEM_OUTPUT="$DIST/GearLeverR-${VERSION}-${ARCH}.AppImage"
BUNDLED_OUTPUT="$DIST/GearLeverR-${VERSION}-${ARCH}-gtk-included.AppImage"
APPIMAGETOOL=${APPIMAGETOOL:-appimagetool}
URUNTIME=${URUNTIME:-$ROOT/.cache/uruntime-appimage-squashfs-lite-$ARCH}
DWARFS=${DWARFS:-$ROOT/.cache/dwarfsextract-$ARCH}
LINUXDEPLOY=${LINUXDEPLOY:-$ROOT/.cache/linuxdeploy-x86_64.AppImage}
LINUXDEPLOY_PLUGIN=${LINUXDEPLOY_PLUGIN:-$ROOT/.cache/linuxdeploy-plugin-gtk.sh}

export APPIMAGE_EXTRACT_AND_RUN=${APPIMAGE_EXTRACT_AND_RUN:-1}

if ! command -v "$APPIMAGETOOL" >/dev/null 2>&1 && [ ! -x "$APPIMAGETOOL" ]; then
    echo "error: appimagetool not found; set APPIMAGETOOL=/path/to/appimagetool" >&2
    exit 1
fi

ARCH="$ARCH" "$ROOT/packaging/appimage/fetch-uruntime.sh" "$URUNTIME"
ARCH="$ARCH" "$ROOT/packaging/appimage/fetch-dwarfs.sh" "$DWARFS"

if [ -n "${GEARLEVER_R_BINARY:-}" ]; then
    BINARY="$GEARLEVER_R_BINARY"
    echo ":: using prebuilt binary $BINARY"
else
    echo ":: building gearlever-r ${VERSION} for ${ARCH}"
    cargo build --release --locked -p gearlever --manifest-path "$ROOT/Cargo.toml"
    BINARY="$ROOT/target/release/gearlever-r"
fi

if [ ! -x "$BINARY" ]; then
    echo "error: binary not found or not executable: $BINARY" >&2
    exit 1
fi

install_common() {
    local appdir=$1
    local variant=$2

    install -Dm644 "$ROOT/data/io.github.alarez777.GearLeverR.desktop" \
        "$appdir/io.github.alarez777.GearLeverR.desktop"
    printf '\nX-GearLeverR-Variant=%s\nX-AppImage-Version=%s\n' "$variant" "$VERSION" \
        >>"$appdir/io.github.alarez777.GearLeverR.desktop"
    install -Dm644 "$ROOT/data/icons/io.github.alarez777.GearLeverR.png" \
        "$appdir/io.github.alarez777.GearLeverR.png"
    ln -sf io.github.alarez777.GearLeverR.png "$appdir/.DirIcon"

    for size in 16 24 32 48 64 128 256 512; do
        install -Dm644 "$ROOT/data/icons/hicolor/${size}x${size}/apps/io.github.alarez777.GearLeverR.png" \
            "$appdir/usr/share/icons/hicolor/${size}x${size}/apps/io.github.alarez777.GearLeverR.png"
    done

    install -Dm644 "$ROOT/data/io.github.alarez777.GearLeverR.gschema.xml" \
        "$appdir/usr/share/glib-2.0/schemas/io.github.alarez777.GearLeverR.gschema.xml"

    install -Dm644 "$ROOT/LICENSES/zsync-rs-MIT.txt" \
        "$appdir/usr/share/doc/gearlever-r/zsync-rs-MIT.txt"
    install -Dm644 "$ROOT/LICENSES/uruntime-MIT.txt" \
        "$appdir/usr/share/doc/gearlever-r/uruntime-MIT.txt"
    install -Dm644 "$ROOT/LICENSES/dwarfs-MIT.txt" \
        "$appdir/usr/share/doc/gearlever-r/dwarfs-MIT.txt"

    # DwarFS metadata needs an extractor; bundle the pinned static binary so
    # the safe path works even when the host has no dwarfsextract installed.
    install -Dm755 "$DWARFS" "$appdir/usr/bin/dwarfsextract"

    if command -v glib-compile-schemas >/dev/null 2>&1; then
        glib-compile-schemas "$appdir/usr/share/glib-2.0/schemas"
    else
        echo "warning: glib-compile-schemas not found; settings will fall back to defaults" >&2
    fi

    if command -v msgfmt >/dev/null 2>&1; then
        "$ROOT/scripts/compile-translations.sh" "$appdir/usr/share/locale"
    else
        echo "warning: msgfmt not found; the AppImage will only show English" >&2
    fi

    install -Dm755 "$ROOT/packaging/appimage/AppRun" "$appdir/AppRun"
}

install_bundled_resources() {
    local appdir=$1

    if [ -d /usr/share/icons/Adwaita ]; then
        mkdir -p "$appdir/usr/share/icons"
        cp -a /usr/share/icons/Adwaita "$appdir/usr/share/icons/"
    else
        echo "warning: Adwaita icon theme not found; the bundled AppImage will miss stock icons" >&2
    fi

    if command -v gtk4-update-icon-cache >/dev/null 2>&1; then
        gtk4-update-icon-cache -f -t "$appdir/usr/share/icons/Adwaita" >/dev/null 2>&1 || true
        gtk4-update-icon-cache -f -t "$appdir/usr/share/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

package_appimage() {
    local appdir=$1
    local output=$2
    local label=$3

    mkdir -p "$DIST"
    echo ":: packaging $label AppImage: $output"
    ARCH="$ARCH" "$APPIMAGETOOL" --runtime-file "$URUNTIME" "$appdir" "$output"

    local offset
    local runtime_size
    offset=$("$output" --appimage-offset)
    runtime_size=$(stat -c %s "$URUNTIME")
    if [ "$offset" != "$runtime_size" ]; then
        echo "error: $output reports payload offset $offset, expected $runtime_size" >&2
        exit 1
    fi

    (cd "$DIST" && sha256sum "$(basename "$output")" >"$(basename "$output").sha256")
}

ensure_linuxdeploy() {
    if [ ! -x "$LINUXDEPLOY" ]; then
        echo ":: downloading linuxdeploy"
        mkdir -p "$(dirname "$LINUXDEPLOY")"
        curl -fsSL -o "$LINUXDEPLOY" \
            https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
        chmod +x "$LINUXDEPLOY"
    fi

    if [ ! -x "$LINUXDEPLOY_PLUGIN" ]; then
        echo ":: downloading linuxdeploy-plugin-gtk"
        mkdir -p "$(dirname "$LINUXDEPLOY_PLUGIN")"
        curl -fsSL -o "$LINUXDEPLOY_PLUGIN" \
            https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh
        chmod +x "$LINUXDEPLOY_PLUGIN"
    fi

    # linuxdeploy discovers plugins next to its own AppImage, with or without
    # the .sh suffix depending on the version.
    ln -sf "$(basename "$LINUXDEPLOY_PLUGIN")" \
        "$(dirname "$LINUXDEPLOY_PLUGIN")/linuxdeploy-plugin-gtk"
}

build_system() {
    echo ":: assembling the default variant (system GTK)"
    rm -rf "$SYSTEM_APPDIR"
    install -Dm755 "$BINARY" "$SYSTEM_APPDIR/gearlever-r"
    install_common "$SYSTEM_APPDIR" "default"
    package_appimage "$SYSTEM_APPDIR" "$SYSTEM_OUTPUT" "default"
}

build_bundled() {
    echo ":: assembling the gtk-included variant"
    ensure_linuxdeploy
    rm -rf "$BUNDLED_APPDIR"

    local extra_libs=()
    local adwaita_libdir
    adwaita_libdir=$(pkg-config --variable=libdir libadwaita-1 2>/dev/null || true)
    if [ -n "$adwaita_libdir" ] && [ -e "$adwaita_libdir/libadwaita-1.so.0" ]; then
        extra_libs=(--library "$adwaita_libdir/libadwaita-1.so.0")
    fi

    DEPLOY_GTK_VERSION=4 "$LINUXDEPLOY" \
        --appdir "$BUNDLED_APPDIR" \
        --executable "$BINARY" \
        --desktop-file "$ROOT/data/io.github.alarez777.GearLeverR.desktop" \
        --icon-file "$ROOT/data/icons/io.github.alarez777.GearLeverR.png" \
        "${extra_libs[@]}" \
        --plugin gtk

    if ! find "$BUNDLED_APPDIR" -name 'libgtk-4.so*' -print -quit | grep -q .; then
        echo "error: linuxdeploy did not bundle libgtk-4" >&2
        exit 1
    fi

    # GLib and the system libraries its host modules link against are provided
    # by the distribution. Bundling them hides the host's GIO modules: the
    # dconf backend then fails to load, which silently turns every GSettings
    # key into its schema default (window buttons, fonts, icon theme) and stops
    # the app settings from persisting. The GTK plugin copies them anyway, so
    # drop them afterwards.
    local system_libs=(
        'libglib-2.0.so*'
        'libgio-2.0.so*'
        'libgobject-2.0.so*'
        'libgmodule-2.0.so*'
        'libmount.so*'
        'libblkid.so*'
        'libselinux.so*'
        'libpcre2-8.so*'
        'libffi.so*'
    )

    local lib
    for lib in "${system_libs[@]}"; do
        find "$BUNDLED_APPDIR" -name "$lib" -delete
    done

    for lib in "${system_libs[@]}"; do
        if find "$BUNDLED_APPDIR" -name "$lib" -print -quit | grep -q .; then
            echo "error: linuxdeploy bundled $lib; it must come from the host" >&2
            exit 1
        fi
    done

    install_common "$BUNDLED_APPDIR" "gtk-included"
    install_bundled_resources "$BUNDLED_APPDIR"
    package_appimage "$BUNDLED_APPDIR" "$BUNDLED_OUTPUT" "gtk-included"
}

case "$VARIANT" in
    default|system|required)
        build_system
        ;;
    bundled|included)
        build_bundled
        ;;
    all)
        build_system
        build_bundled
        ;;
    *)
        echo "error: VARIANT must be default, system, bundled or all (got '$VARIANT')" >&2
        exit 1
        ;;
esac

echo ":: done"
