#!/usr/bin/env bash
# Live code hotpatching + shader hot-reloading (experimental): body edits apply
# live, struct/signature/plugin changes need a restart. Needs the Dioxus CLI 0.7.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=scripts/_cargo_target.sh
. scripts/_cargo_target.sh

if ! command -v dx >/dev/null 2>&1; then
    echo "ERROR: the Dioxus CLI (dx) is not installed." >&2
    echo "Install it (no build needed) with:" >&2
    echo "  curl -sL -o /tmp/dx.tar.gz https://github.com/DioxusLabs/dioxus/releases/download/v0.7.10/dx-x86_64-unknown-linux-gnu.tar.gz" >&2
    echo "  tar xzf /tmp/dx.tar.gz -C /tmp && install -m755 /tmp/dx ~/.cargo/bin/dx" >&2
    exit 1
fi

export BEVY_ASSET_ROOT="$PWD"
export CARGO_INCREMENTAL=1

# Wayland by default; REF_X11=1 forces Xwayland. Env knobs: REF_FRAME_LATENCY,
# REF_PRESENT_MODE, REF_ACCENT.
if [ "${REF_X11:-0}" = "1" ] && [ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${DISPLAY:-}" ]; then
    echo "refcanvas: forcing the X11 (Xwayland) backend (REF_X11=1)."
    unset WAYLAND_DISPLAY
fi

exec dx serve --hot-patch --features hotpatch "$@"
