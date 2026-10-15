# Sourced by the build/run scripts (not run directly). Picks the build cache:
# the SSD from .cargo/config.toml when mounted, else the default ./target.
# Override the mount with REF_PORTABLE_ROOT.
PORTABLE_ROOT=${REF_PORTABLE_ROOT:-/mnt/portablessd}

if command -v mountpoint >/dev/null 2>&1 && mountpoint -q "$PORTABLE_ROOT"; then
    : # mounted: .cargo/config.toml already points the target dir at it
else
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
    if [[ -f .cargo/config.toml ]]; then
        echo "refcanvas: $PORTABLE_ROOT not mounted, using CARGO_TARGET_DIR=$CARGO_TARGET_DIR" >&2
    fi
fi
