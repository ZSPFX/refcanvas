# Vendored dependencies

## `winit/` — winit 0.30.13 + Wayland drag-and-drop (PR #4269)

Bevy 0.20 uses winit 0.30, whose Wayland backend has **no** drag-and-drop
implementation: `WindowEvent::DroppedFile` / `HoveredFile` are only emitted by
the X11, macOS and Windows backends, so file drops silently do nothing on
native Wayland (they work under Xwayland, hence Electron apps "just working").

`vendor/winit` is an unmodified checkout of winit `v0.30.13` with the
(unmerged, community) v0.30.x pull request applied:

- https://github.com/rust-windowing/winit/pull/4269
- "Add support for file window events on Wayland" — adds the
  `wl_data_device` (SCTK) plumbing, accepts `text/uri-list`, and emits the
  regular `HoveredFile` / `DroppedFile` events.

On top of the PR, one local fix: the PR only parsed the **first** line of the
`text/uri-list` and kept a single path per drop, so dragging several files
imported one. `DndOfferState` now holds `paths: Vec<PathBuf>`, every URI line
is parsed, and one `DroppedFile` is emitted per file.

Wired up in `Cargo.toml` via:

```toml
[patch.crates-io]
winit = { path = "vendor/winit" }
```

Nothing in the app needed changing — `project::import::handle_dropped_files`
already consumes `FileDragAndDrop`.

### Regenerating the patch

```sh
git clone --depth 1 --branch v0.30.13 https://github.com/rust-windowing/winit
cd winit && curl -sL https://github.com/rust-windowing/winit/pull/4269.diff | git apply
```

### Removing it

Delete this directory and the `[patch.crates-io]` section once Bevy depends
on a winit that ships Wayland DnD itself (0.31+).
