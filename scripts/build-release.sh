#!/usr/bin/env bash
# Release build: compile once, then write the artifacts to dist/:
#   refcanvas-<version>-<pkgrel>-x86_64.pkg.tar.zst   (Arch: sudo pacman -U)
#   refcanvas-<version>-x86_64.AppImage               (everything else)
#   refcanvas-<version>.exe                           (Windows, cargo-xwin)
#
# The Arch package is assembled by makepkg from this checkout's working tree (its
# build() reuses the binary built here), so uncommitted changes are fine.
# --linux-only skips the Windows cross-build; any other argument goes to cargo.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=scripts/_cargo_target.sh
. scripts/_cargo_target.sh
# shellcheck source=scripts/_packaging.sh
. scripts/_packaging.sh

LINUX_ONLY=0
CARGO_ARGS=()
for arg in "$@"; do
    case $arg in
        --linux-only) LINUX_ONLY=1 ;;
        *) CARGO_ARGS+=("$arg") ;;
    esac
done

# One metadata read for both the target dir and the package version. (No build
# needed for this, so a version mismatch fails before compiling anything.)
read -r TARGET_DIR VERSION <<<"$(cargo metadata --format-version=1 --no-deps 2>/dev/null \
    | python3 -c 'import sys,json; m=json.load(sys.stdin); print(m["target_directory"], m["packages"][0]["version"])')"

# Keep the Arch package's pkgver in step with Cargo.toml. makepkg reads PKGBUILD
# from the working tree, so this applies immediately (no commit needed).
PKGVER=$(awk -F= '/^pkgver=/{print $2; exit}' packaging/PKGBUILD)
if [[ $PKGVER != "$VERSION" ]]; then
    sed -i "s/^pkgver=.*/pkgver=$VERSION/" packaging/PKGBUILD
    echo "refcanvas: packaging/PKGBUILD pkgver: $PKGVER -> $VERSION"
fi

cargo build --release --locked "${CARGO_ARGS[@]}"

DIST="$PWD/dist"
STAGE="$DIST/.stage"
# Start clean, so a failed run can't leave anything behind to confuse the next.
rm -rf "$STAGE"
mkdir -p "$STAGE"

# makepkg's build() copies these into its (HEAD) clone so the package gets the
# working tree's icons instead of whatever was last committed.
export REF_ASSETS_DIR="$PWD/assets/icons"

# Strip once: both artifacts get this exact binary.
BIN="$STAGE/refcanvas"
install -m755 "$TARGET_DIR/release/refcanvas" "$BIN"
strip "$BIN"

PKG=$(pack_pacman "$BIN")
APPIMAGE=$(pack_appimage "$BIN" "$VERSION")

WIN=skipped
if [[ $LINUX_ONLY = 1 ]]; then
    echo "refcanvas: --linux-only: skipping the Windows build." >&2
    if compgen -G "$DIST/*.exe" >/dev/null; then
        echo "refcanvas: note: dist/ still holds a .exe from an earlier run - not rebuilt." >&2
    fi
else
    WIN=$(pack_windows "$TARGET_DIR" "$VERSION")
fi

rm -rf "$STAGE"

echo
echo "Release ready in dist/:"
FILES=("$PKG" "$APPIMAGE")
if [[ $WIN != skipped ]]; then
    FILES+=("$WIN")
fi
for f in "${FILES[@]}"; do
    printf '  %-46s %s\n' "${f#"$DIST"/}" "$(du -h "$f" | cut -f1)"
done
