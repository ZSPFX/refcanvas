# <sub><img src="https://cdn.zspfx.com/refcanvas/icon.svg" width="32" height="32" alt=""></sub> refcanvas

A GPU-accelerated infinite canvas for digital artists, written in Rust + Bevy 0.20 with Vulkan rendering.

![Picture](https://cdn.zspfx.com/refcanvas/app_0.1.2-3.webp)

> Heads-up, LLM were used to built this; it is marely as a tool to translate my technical decisions into a code, I took full control over the design and software artitecture to ensure best possible quality and smooth experience using my software engineering knowledge.

## Design Goals

> I've always disliked how there's not many choices for local canvas app, there was Obisidan but the performance is terrible (constant stutters and greedy VRAM usage) thanks to Electron for that... so I just build one myself that I'll use for my projects as a 3D Generalist.

- Butterly smooth performance on canvas with thousands of images
- Gizmos and interactions that felt just right
- Free up VRAM whenever possible (More memory for DCC)
- Local-First design without relying on the Cloud

## Features

- Drag & drop images
- Portable project folder / Open multiple projects as tabs
- Copy image to clipboard and paste from clipboard
- Crisp image pixels when deep zoom
- Gizmos control for single/multi-selection transformation
- Move/Scale/Crop smart snapping on visible images
- Arrow key focused image navigations
- Focus on selected images [F]
- Frame group of images [CTRL+G]
- Industry standard hotkeys
- Autosave, Undo and Redo [Z, CTRL+Y]
- Hold thousands of images in one canvas (5,000+ OK!)

## Technical Features

> Benchmarking can be done by clicking on bottom right FPS label to uncap the 120FPS limit.

- First-class Linux support, also runs on Windows
- Vulkan backend renderer performance, benchmarked result with (2K~4K images scattered in clusters)
  - 1000~ FPS, 100 images
  - 900~ FPS, 200 images
  - 800~ FPS, 500 images
  - 200~ FPS, 5,000 images
- Automatic VRAM eviction
- Arrow key image navigations with ray casting, distance, and viewport boundary
- 0% CPU/GPU utilization while idle/unfocused
- Previews (512px) LOD generation

## Not Yet Supported / Planned

- OpenColorIO
- Texts
- Animated GIF
- Videos
- Compare A/B test slider
- Arrangement compaction
- Change frame color
- Notify new version release
- DB Lock

## Download & Installation

### Linux

For Arch Linux you can download the tar file from [releases](https://github.com/zspfx/refcanvas/releases) then install with `sudo pacman -U ./refcanvas...x86_64.pkg.tar.zst`

For other Linux distros you can find the AppImage in [releases](https://github.com/zspfx/refcanvas/releases) page.

### Windows

For Windows installation see the [releases](https://github.com/zspfx/refcanvas/releases) page for pre-built executable.

## License

Copyright (C) 2026 ZSPFX. RefCanvas is free software under **GPL-3.0-or-later**;
see [LICENSE](LICENSE) for the full text.
