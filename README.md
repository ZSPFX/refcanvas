# <sub><img src="https://cdn.zspfx.com/refcanvas/icon.svg" width="32" height="32" alt=""></sub> refcanvas

A GPU-accelerated infinite canvas for digital artists, written in Rust using Bevy 0.20 (Vulkan backend).

![Picture](https://cdn.zspfx.com/refcanvas/app_0.1.2-3.webp)

> Heads-up: LLM were used to build this, it's marely a tool to translate my technical decisions into code, I took full control over the design and architecture using my software engineering knowledge.

> `README.md` and commits are fully written by me to ensure quality and human readability.

## Design Goals

- Support multiple artists accessing the same project on shared drive. (compatible with Syncthing and SMB!)
- Stable and smooth canvas performance with thousands of images.
- Free up GPU VRAM whenever possible.
- Hotkeys and gizmos that felt just right.
- Introduce industry-standard hotkeys.

## Features

- App
  - Portable project folder.
  - Open multiple projects as tabs.
  - Support Syncthing/SMB shared folder location and multiple user viewing/editing at the same time.
  - Always-on-top pin for keeping references above other apps.
  - Support thousands of images in one canvas. (5,000+ OK!)
  - Touchscreen support. (Surface devices)

- Canvas
  - Drag and drop images.
  - Copy/paste images from/to the clipboard.
  - Crisp pixels when zoomed in deeply.
  - Gizmos control for single and multi selection.
  - Smart snapping that only snaps on visible images.
  - Arrow key navigations on focused image.
  - Focus on selected images. [F]
  - Frame group of images. [CTRL+G]
  - Automatic saving, Undo and Redo. [Z, CTRL+Y]

## Technical Features

> Benchmarking can be done by clicking the bottom right FPS label to uncap the 120FPS limit.

- First-class Linux support, also runs on Windows and MacOS.
- Shared folder (per-device append-only journal and automatic conflict resolution).
- Vulkan backend renderer performance, benchmarked result with 2K~4K images scattered in clusters.
  - 1000~ FPS, 100 images.
  - 900~ FPS, 200 images.
  - 800~ FPS, 500 images.
  - 200~ FPS, 5000 images.
- LOD previews generation.
- Automatic VRAM eviction when LOD is active.
- 0% CPU/GPU utilization while idle/unfocused.
- Arrow key image navigations with ray casting, distance, and viewport boundary.

## Not Yet Supported / Planned features

- OpenColorIO.
- Texts.
- Animated GIF.
- Videos.
- Compare A/B test slider.
- Arrangement compaction.
- Change frame color.

## Download & Installation

### Linux

Arch Linux users may download the tar file from [releases](https://github.com/zspfx/refcanvas/releases) then install with `sudo pacman -U ./refcanvas...x86_64.pkg.tar.zst`.
Other distros may find the AppImage from the same page as well.

### Windows

Windows users may find the precompiled executable from [releases](https://github.com/zspfx/refcanvas/releases).

### MacOS (Apple Silicon)

Dawin-only artifacts will be compiled through GitHub Actions shortly after the new release is published, you may find them at [releases](https://github.com/zspfx/refcanvas/releases). Extract the zip first then move `RefCanvas.app` into `/Applications`, then remove the quarantine flag using `xattr -cr /Applications/RefCanvas.app`.

## License

Copyright (C) 2026 ZSPFX. RefCanvas is free software under **GPL-3.0-or-later**;
see [LICENSE](LICENSE) for the full text.
