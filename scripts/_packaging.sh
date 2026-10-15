# Sourced by scripts/build-release.sh (not run directly): turns one already
# built + stripped binary into the two release artifacts. Expects $DIST, $STAGE
# and $REF_ASSETS_DIR to be set and $PWD to be the repo root.
#
#  - Arch: makepkg only *assembles*: packaging/PKGBUILD's build() reuses
#    REF_PREBUILT_BIN instead of compiling, so nothing is built twice.
#  - AppImage: the AppDir is hand-assembled and handed to appimagetool. No
#    linuxdeploy: the release binary is standalone (the grid shader is baked
#    in), so there is nothing to bundle but the binary + desktop + icon.

APPIMAGE_TOOL_CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/refcanvas-tools"

# Print the path to appimagetool for the given arch, downloading it once.
appimagetool_path() {
    local arch=$1 tool url
    tool="$APPIMAGE_TOOL_CACHE/appimagetool-$arch.AppImage"
    if [[ ! -x $tool ]]; then
        url="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$arch.AppImage"
        echo "refcanvas: fetching appimagetool ($arch) -> $tool" >&2
        mkdir -p "$APPIMAGE_TOOL_CACHE"
        curl -fL --retry 3 -o "$tool.part" "$url"
        mv -f "$tool.part" "$tool"
        chmod +x "$tool"
    fi
    printf '%s\n' "$tool"
}

# Same for the type-2 runtime: appimagetool re-downloads it on every run
# otherwise, which would make the pipeline need the network each time.
appimage_runtime_path() {
    local arch=$1 runtime
    runtime="$APPIMAGE_TOOL_CACHE/runtime-$arch"
    if [[ ! -s $runtime ]]; then
        echo "refcanvas: fetching the AppImage runtime ($arch) -> $runtime" >&2
        mkdir -p "$APPIMAGE_TOOL_CACHE"
        curl -fL --retry 3 -o "$runtime.part" \
            "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-$arch"
        mv -f "$runtime.part" "$runtime"
    fi
    printf '%s\n' "$runtime"
}

# pack_appimage <binary> <version> -> prints the artifact path
pack_appimage() {
    local bin=$1 ver=$2
    local arch=x86_64
    local appdir="$STAGE/refcanvas.AppDir"
    local out="$DIST/refcanvas-$ver-$arch.AppImage"

    rm -rf "$appdir"
    install -Dm755 "$bin" "$appdir/usr/bin/refcanvas"
    install -Dm644 "$PWD/packaging/refcanvas.desktop" "$appdir/refcanvas.desktop"

    # appimagetool picks the root icon up as the image's icon; the same sizes as
    # the Arch package go in the theme dir for desktop integration.
    for size in 16 22 24 32 48 64 128 256 512; do
        install -Dm644 "$PWD/assets/icons/refcanvas-$size.png" \
            "$appdir/usr/share/icons/hicolor/${size}x${size}/apps/refcanvas.png"
    done
    install -m644 "$PWD/assets/icons/refcanvas-256.png" "$appdir/refcanvas.png"
    cp "$appdir/refcanvas.png" "$appdir/.DirIcon"

    # A shim instead of a symlink: $0 is AppRun inside the mounted image.
    cat >"$appdir/AppRun" <<'EOF'
#!/bin/sh
exec "$(dirname "$(readlink -f "$0")")/usr/bin/refcanvas" "$@"
EOF
    chmod +x "$appdir/AppRun"

    # APPIMAGE_EXTRACT_AND_RUN: run without depending on FUSE / libfuse2.
    # --no-appstream: we ship no AppStream metainfo (appimagetool would warn).
    rm -f -- "$out"
    APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$arch" "$(appimagetool_path "$arch")" \
        --no-appstream --runtime-file "$(appimage_runtime_path "$arch")" \
        "$appdir" "$out" >&2

    printf '%s\n' "$out"
}

# pack_windows <target-dir> <version> -> prints the artifact path
pack_windows() {
    local target_dir=$1 ver=$2
    local target=x86_64-pc-windows-msvc

    command -v cargo-xwin >/dev/null || {
        echo "refcanvas: cargo-xwin is missing, install it with:" >&2
        echo "  cargo install cargo-xwin" >&2
        return 1
    }
    rustup target list --installed | grep -qx "$target" || {
        echo "refcanvas: the Windows std is missing, add it with:" >&2
        echo "  rustup target add $target" >&2
        return 1
    }

    # Cross-compiled with cargo-xwin (MSVC). build.rs links the .ico in.
    if ! cargo xwin build --release --locked --target "$target" >&2; then
        echo "refcanvas: Windows build failed." >&2
        return 1
    fi

    local out="$DIST/refcanvas-$ver.exe"
    install -Dm755 "$target_dir/$target/release/refcanvas.exe" "$out"
    printf '%s\n' "$out"
}

# pack_pacman <binary> -> prints the artifact path
pack_pacman() {
    local bin=$1 pkgfile
    # Keep every makepkg tree — the HEAD mirror (SRCDEST), the extracted copy +
    # package dir (BUILDDIR) and the logs — inside the staging dir rather than
    # scattering src/, pkg/ and refcanvas/ through packaging/. build-release.sh
    # deletes the staging dir when it's done, so packaging/ stays as checked in.
    # SRCDEST and BUILDDIR must be different: makepkg mirrors into
    # $SRCDEST/$pkgname and extracts the working copy into $srcdir/$pkgname.
    local work="$STAGE/makepkg"
    mkdir -p "$work"

    pushd "$PWD/packaging" >/dev/null
    # Make the rustup shims win over a minimal PATH (makepkg's own env).
    export PATH="$HOME/.cargo/bin:$PATH"
    pkgfile=$(PKGDEST="$DIST" SRCDEST="$work/source" BUILDDIR="$work/build" \
        makepkg --packagelist)
    # -C: start from a fresh clone of the committed HEAD (build() refreshes the
    # icons from the working tree, so uncommitted icons are fine). -f: rebuild
    # even if the package already exists. build() reuses $bin, so nothing is
    # compiled. The `if` keeps makepkg failures fatal.
    if ! REF_PREBUILT_BIN="$bin" PKGDEST="$DIST" \
        SRCDEST="$work/source" BUILDDIR="$work/build" LOGDEST="$work/log" \
        makepkg -C -f --nodeps --noconfirm --noprogressbar >&2; then
        popd >/dev/null
        echo "refcanvas: makepkg failed." >&2
        return 1
    fi
    popd >/dev/null
    printf '%s\n' "$pkgfile"
}
