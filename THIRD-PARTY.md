# Third-party notices

RefCanvas is licensed under the **GNU General Public License v3.0 or later**
(see [LICENSE](LICENSE)). It is built on open-source components that remain
under their own licenses, reproduced/referenced here for binary redistribution.

## Direct dependencies

| Component | License |
| --- | --- |
| [Bevy](https://bevyengine.org) | MIT OR Apache-2.0 |
| [bevy_egui](https://github.com/vleue/bevy_egui) | MIT |
| [arboard](https://github.com/1Password/arboard) | MIT OR Apache-2.0 |
| [image](https://github.com/image-rs/image) | MIT OR Apache-2.0 |
| [blake3](https://github.com/BLAKE3-team/BLAKE3) | CC0-1.0 OR Apache-2.0 |
| [rusqlite](https://github.com/rusqlite/rusqlite) | MIT |
| [rfd](https://github.com/PolyMeilex/rfd) | MIT |
| [directories](https://github.com/dirs-dev/directories-rs) | MIT OR Apache-2.0 |
| [serde](https://serde.rs) / serde_json | MIT OR Apache-2.0 |
| [anyhow](https://github.com/dtolnay/anyhow) | MIT OR Apache-2.0 |
| windows-sys | MIT OR Apache-2.0 |
| [winit](https://github.com/rust-windowing/winit) (vendored in `vendor/winit`) | Apache-2.0 |

## Bundled fonts

- `epaint_default_fonts` (pulled in by `bevy_egui`) is licensed
  `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`.
- Bevy's built-in default font (`bevy_text`) ships under its bundled font
  licenses; see the `bevy_text` crate for the exact terms.

## Transitive dependencies

The complete dependency tree and each crate's declared license are recorded in
[Cargo.lock](Cargo.lock) (`cargo metadata` is authoritative). Everything is
permissive (MIT / Apache-2.0 / BSD / ISC / Zlib / Unicode-3.0 / CC0 / Unlicense).
The only copyleft entries are `option-ext` (MPL-2.0) and `self_cell`
(Apache-2.0 OR GPL-2.0-only, used here under Apache-2.0); both are compatible
with GPL-3.0.

When redistributing a compiled binary, include the license texts above (and
those listed in `Cargo.lock`) with the distribution.
