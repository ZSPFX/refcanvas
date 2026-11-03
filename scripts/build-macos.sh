#!/usr/bin/env bash
# macOS release build (Apple Silicon only): compiles once and writes
#   dist/refcanvas-<version>-macos-arm64.zip   (RefCanvas.app, ad-hoc signed)
#
# Run on a Mac or in CI; needs Xcode command line tools for iconutil and
# codesign, which macOS runners already provide. There is no cross-compile step
# and no Intel slice, so the artifact runs on arm64 machines only.
#
# Env knobs:
#   MACOS_SIGN_IDENTITY   Developer ID identity; defaults to ad-hoc ("-")
#   MACOSX_DEPLOYMENT_TARGET  minimum macOS version (default 11.0)
set -euo pipefail
cd "$(dirname "$0")/.."

# Kept bash 3.2 compatible: macOS ships that version by default.
TARGET=aarch64-apple-darwin
DEPLOY_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}

[[ "$(uname -s)" == Darwin ]] || {
    echo "ERROR: build-macos.sh must run on macOS." >&2
    exit 1
}

for tool in iconutil codesign ditto; do
    command -v "$tool" >/dev/null || {
        echo "ERROR: $tool is missing, install the Xcode command line tools." >&2
        exit 1
    }
done

# One metadata read for both the target dir and the package version, so a
# version mismatch fails before compiling anything.
METADATA=$(cargo metadata --format-version=1 --no-deps 2>/dev/null)
TARGET_DIR=$(printf '%s' "$METADATA" | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')
VERSION=$(printf '%s' "$METADATA" | python3 -c 'import sys,json; print(json.load(sys.stdin)["packages"][0]["version"])')

# An explicit tag (CI) must match Cargo.toml, otherwise the wrong binary could
# be attached to a release.
if [[ -n "${REF_RELEASE_TAG:-}" && "${REF_RELEASE_TAG#v}" != "$VERSION" ]]; then
    echo "ERROR: release tag ${REF_RELEASE_TAG} does not match Cargo.toml version $VERSION." >&2
    exit 1
fi

rustup target list --installed | grep -qx "$TARGET" || {
    echo "refcanvas: adding the $TARGET target" >&2
    rustup target add "$TARGET"
}

echo "refcanvas: building $VERSION for $TARGET (macOS $DEPLOY_TARGET+)"
MACOSX_DEPLOYMENT_TARGET="$DEPLOY_TARGET" \
    cargo build --release --locked --target "$TARGET"

DIST="$PWD/dist"
STAGE="$DIST/.stage-macos"
APP="$STAGE/RefCanvas.app"
CONTENTS="$APP/Contents"

rm -rf "$STAGE"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"

install -m755 "$TARGET_DIR/$TARGET/release/refcanvas" "$CONTENTS/MacOS/refcanvas"

# AppIcon.icns from the committed PNGs; 1024 px is the required @2x base.
ICONSET="$STAGE/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    src="$PWD/assets/icons/refcanvas-$size.png"
    [[ -f $src ]] || {
        echo "ERROR: $src is missing, run scripts/make-icons.sh first." >&2
        exit 1
    }
    cp "$src" "$ICONSET/icon_${size}x${size}.png"
    cp "$src" "$ICONSET/icon_${size}x${size}@2x.png"
done
cp "$PWD/assets/icons/refcanvas-1024.png" "$ICONSET/icon_512x512@2x.png"
iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
rm -rf "$ICONSET"

cat >"$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>refcanvas</string>
    <key>CFBundleExecutable</key>
    <string>refcanvas</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.zspfx.refcanvas</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>refcanvas</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>
    <string>$DEPLOY_TARGET</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
PLIST
printf 'APPL????' >"$CONTENTS/PkgInfo"

# An explicit identity signs with hardened runtime; otherwise ad-hoc signing
# keeps the arm64 binary runnable on the machine that downloads it.
if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
    echo "refcanvas: signing with $MACOS_SIGN_IDENTITY"
    codesign --force --deep --options runtime --timestamp \
        --sign "$MACOS_SIGN_IDENTITY" "$APP"
else
    codesign --force --deep --sign - "$APP"
fi
codesign --verify --deep --strict "$APP"

OUT="$DIST/refcanvas-$VERSION-macos-arm64.zip"
rm -f "$OUT"
# ditto keeps the bundle metadata and symlinks; zip alone does not.
ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT"
rm -rf "$STAGE"

echo
echo "Release ready in dist/:"
printf '  %-46s %s\n' "${OUT#"$DIST"/}" "$(du -h "$OUT" | cut -f1)"
