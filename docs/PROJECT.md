One project = one folder containing a single SQLite `.db`, the copied reference
images, and a regenerable cache. Open as many projects as you like in
Chrome-style tabs.

```
MyProject/
├── project.db     canvas state (nodes, transforms, camera)
├── images/        originals, copied on import (content-hash dedup)
└── cache/         generated previews, safe to delete, regenerated on demand
```

## Development

The build cache normally lives on the portable SSD
(`/mnt/portablessd/refcanvas-target`, configured in the gitignored
`.cargo/config.toml`). The scripts use it when it is mounted and otherwise fall
back to the default `target/` dir, so a fresh checkout builds anywhere; set
`REF_PORTABLE_ROOT` to point at a different cache location.

```bash
./scripts/hotpatch.sh            # run with live hotpatching + shader reloading
cargo run --features dev_dylib   # run a normal dev build (fast relinks)
./scripts/build-release.sh       # release artifacts -> dist/ (Arch pkg + AppImage + exe)
./scripts/build-release.sh --linux-only   # same, skipping the Windows cross-build
./scripts/make-icons.sh          # regenerate the icon rasters from icon.svg
```

`build-release.sh` compiles once, strips once, then produces every artifact in
`dist/`: makepkg only *assembles* the Arch package (`PKGBUILD`'s `build()`
reuses the binary via `REF_PREBUILT_BIN` and refreshes the icons from the working
tree), the AppImage is a hand-assembled AppDir handed to `appimagetool` (fetched
once into `~/.cache/refcanvas-tools/`), and the Windows `.exe` is cross-compiled
with cargo-xwin (`cargo install cargo-xwin` + `rustup target add
x86_64-pc-windows-msvc`). Everything is packaged from the **working tree**, so no
commit is required; `PKGBUILD`'s `pkgver` is synced from `Cargo.toml` on the way.
Install the package with:

```bash
sudo pacman -U dist/refcanvas-*.pkg.tar.zst
```

The Windows `.exe` links the MSVC runtime dynamically, so it needs the VC++
Redistributable on the target machine (`api-ms-win-crt-*` is part of Windows
10+). Build it with `-C target-feature=+crt-static` in `RUSTFLAGS` for a fully
standalone exe.

The app icon lives in `assets/icon.svg`. `scripts/make-icons.sh` regenerates the
committed derivatives from it: `assets/icons/refcanvas-<size>.png` (installed by
both Linux artifacts and desktop themes) and `assets/icons/refcanvas.ico`, which
`build.rs` links into the Windows `.exe` with `llvm-rc`/`rc.exe`.

### Live updates (hotpatching)

`./scripts/hotpatch.sh` runs the app through the Dioxus CLI (`dx serve
--hot-patch`) so you can edit code **while the app is running**:

- **System/observer bodies** are hot-patched live (e.g. tweak the zoom feel,
  snapping thresholds, gizmo drawing, colors, save and watch it change).
- **Shaders / assets** hot-reload too (`file_watcher`): edit
  `assets/shaders/dot_grid.wesl` and the grid updates instantly.
- **Not live:** struct/resource/component layout changes, system signatures,
  adding new systems/plugins, and Startup-only setup, those need a normal
  restart (`cargo run --features dev_dylib`). Autosave + session restore bring
  you back exactly where you were.

Requires the Dioxus CLI **0.7.10+** (one-time, prebuilt, no long compile; the 0.7.0-rc builds cannot launch native apps in this flow):

```bash
curl -sL -o /tmp/dx.tar.gz \
  https://github.com/DioxusLabs/dioxus/releases/download/v0.7.10/dx-x86_64-unknown-linux-gnu.tar.gz
tar xzf /tmp/dx.tar.gz -C /tmp && install -m755 /tmp/dx ~/.cargo/bin/dx
```

Notes: Linux needs clang + lld (already configured in `.cargo/config.toml`);
this is experimental upstream tech (subsecond + `dx serve --hot-patch`), and
the first build with the `hotpatch` feature recompiles Bevy.

The release binary is fully standalone, the grid shader is baked into the
executable at compile time. Debug builds load shaders from `assets/`, so run
those through `cargo run` (or copy `assets/` next to the debug binary).

## Canvas controls

| Input                                    | Action                                                       |
| ---------------------------------------- | ------------------------------------------------------------ |
| Alt+MMB drag / MMB drag / Space+LMB drag | Pan                                                          |
| Alt+RMB drag (horizontal)                | Dolly (zoom)                                                 |
| Mouse wheel                              | Zoom at cursor (very fast easing)                            |
| F                                        | Frame selection (Maya-style; frames all if nothing selected) |
| A                                        | Frame everything                                             |
| Ctrl+0                                   | Reset zoom to 100%                                           |

## Selection & transforms

| Input                   | Action                                                                                                                                                                                                                                                                                          |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| LMB click               | Select (raises the node)                                                                                                                                                                                                                                                                        |
| Shift+LMB               | Add/remove from selection                                                                                                                                                                                                                                                                       |
| LMB drag on empty space | Marquee select (Shift adds)                                                                                                                                                                                                                                                                     |
| LMB drag on a node      | Move (Shift = axis lock, Ctrl = disable snapping)                                                                                                                                                                                                                                               |
| Corner/edge handles     | Resize the selection only, a frame resizes without scaling the pictures inside. Marquee-select the frame **and** the pictures to resize them as a group; images use Figma-style **fill**: the picture rescales uniformly to cover the new box, so it grows whole or crops, but never stretches |
| Shift + handles         | Keep the box's aspect ratio while resizing (Alt = resize from the centre)                                                                                                                                                                                                                       |
| Ctrl + handle drag      | **Precise crop** (single images): the dragged edge trims the visible region, the picture's scale stays put, the opposite edge is anchored (Shift/Alt don't apply here)                                                                                                                         |
| Just outside a corner   | Rotate (Shift = 15° snap)                                                                                                                                                                                                                                                                       |
| Escape                  | Cancel the current drag (restores state) or deselect                                                                                                                                                                                                                                            |
| F                       | Frame selection                                                                                                                                                                                                                                                                                 |

Dragging and resizing snap to the edges/centers of nearby nodes (pink guides
with a tick on every aligned node, only nodes near the viewport can snap).
Hold Ctrl while dragging to disable snapping.

## Editing

| Input                                | Action                                                                                                                                                     |
| ------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Ctrl+Z / Ctrl+Shift+Z (or Ctrl+Y)    | Undo / redo                                                                                                                                                |
| Ctrl+C / Ctrl+V                      | Copy the selection to the system clipboard as an image (crop, flips and rotation baked in) / paste images or files from the system clipboard at the cursor |
| Ctrl+D                               | Duplicate                                                                                                                                                  |
| Delete / Backspace                   | Delete                                                                                                                                                     |
| Ctrl+A                               | Select all (active tab)                                                                                                                                    |
| Ctrl+G                               | Wrap selection in a frame (dragging the frame moves its contents)                                                                                          |
| LMB drag on a frame's text label     | Selects/moves the frame, the label is a grab handle                                                                                                       |
| Double-click a frame's label         | Rename the frame, Enter or clicking away saves, Escape cancels                                                                                            |
| Right-click                          | Context menu (z-order, reset crop/size, flips, align, distribute, …)                                                                                       |
| Click the fps readout (bottom-right) | Toggle benchmark mode: black chip + white text + the 120 fps cap lifted                                                                                    |
| G                                    | Toggle the dot grid                                                                                                                                        |

## Projects & tabs

| Input                             | Action                                               |
| --------------------------------- | ---------------------------------------------------- |
| Ctrl+T                            | New project (choose a folder)                        |
| Ctrl+O                            | Open project (pick a `project.db`)                   |
| Ctrl+W                            | Close tab                                            |
| Right-click a tab                 | Context menu: close the tab, open the project folder |
| Ctrl+Tab / Ctrl+Shift+Tab         | Cycle tabs                                           |
| Ctrl+S                            | Force save (autosave runs every ~1.5 s anyway)       |
| Drop files/folder onto the window | Import images                                        |

Open tabs and the active tab are restored on startup.

## Performance notes

- Previews (512 px) are generated at import; the canvas paints instantly from
  them on reopen. Full-resolution textures stream in on demand for nodes near
  the viewport, are LRU-evicted under a 3 GiB budget, and switch to nearest
  sampling past ~2× texel magnification (deep zoom shows crisp image pixels).
- A 100 × 4K reference stress test runs at 60 fps during import, panning and
  zooming (dev build, RTX 5070 Ti).
- V-Sync is **off**; the loop is software-capped at **120 fps** (`TARGET_FPS` in
  `src/main.rs`) so input latency stays low and the FPS readout is live. While
  the window is being resized the app temporarily presents with `Fifo` (synced
  to the compositor) and lifts the cap, so every resize step is answered with a
  fresh frame, live resizing stays smooth without tearing or swap-chain churn.
- An idle window drops to a **~1 fps heartbeat** (focused or not), but only
  once nothing is loading: preview streaming, imports and LOD loads are polled
  per frame, so `idle_throttle` (`src/main.rs`) keeps the loop at full speed
  until those queues drain. Any input wakes the sleeping loop immediately, and
  an in-flight view ease requests redraws, so zooming stays smooth.
- Image resize handles keep Figma's `object-fit: cover` semantics: the box
  rescales, the picture always covers it uniformly, it grows whole or crops,
  never stretches. **Ctrl + handle** switches to precise crop: drag an edge or
  corner to trim/reveal texture without changing the picture's scale. Crop and
  scale persist in the project db.
- Known trade-off: Bevy's CPU mip-upload path renders incorrectly blurry, so
  textures are single-level; minification quality relies on the pre-antialiased
  preview tier plus on-demand full-res instead of mipmaps.

## Not implemented (yet)

- EXR/HDR sources; Windows builds (code is path-portable, untested there).
- Multi-user note: keep a project's `.db` edited by one person at a time
  (SQLite is not sync-safe); images and previews sync fine via Syncthing.
