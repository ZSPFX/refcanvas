#!/usr/bin/env bash
# Re-render the raster icons from assets/icon.svg:
#   refcanvas-<n>.png   hicolor theme sizes installed by the Linux packages
#   refcanvas.ico       multi-size icon embedded in the Windows .exe
# Only needed when the icon changes: the derived files are committed, so release
# builds need no rasterizer.
set -euo pipefail
cd "$(dirname "$0")/.."

PNG_SIZES=(16 22 24 32 48 64 128 256 512)
ICO_SIZES=(16 24 32 48 64 128 256)

command -v rsvg-convert >/dev/null || {
    echo "ERROR: rsvg-convert is missing, install librsvg." >&2
    exit 1
}
command -v magick >/dev/null || {
    echo "ERROR: magick is missing, install imagemagick." >&2
    exit 1
}

mkdir -p assets/icons
for size in "${PNG_SIZES[@]}"; do
    rsvg-convert -w "$size" -h "$size" -o "assets/icons/refcanvas-$size.png" assets/icon.svg
done

# One .ico holding every size Windows asks for (Explorer, taskbar, alt-tab).
magick assets/icons/refcanvas-256.png -background none \
    -define icon:auto-resize="$(IFS=,; echo "${ICO_SIZES[*]}")" \
    assets/icons/refcanvas.ico

echo "assets/icons: ${PNG_SIZES[*]} px PNGs + refcanvas.ico (${ICO_SIZES[*]})"