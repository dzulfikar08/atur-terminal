# Third-party notices

Atur Terminal is built on the software below. Each package ships this file and LICENSE
(`LICENSE` inside archives and bundles is LICENSE, NOTICE, this file, and the herdr notices
concatenated).

## Project lineage

| Component | Role | Licence | Copyright |
| --- | --- | --- | --- |
| [Muxy](https://github.com/muxy-app/muxy) | Upstream of this fork | MIT | Copyright (c) 2026 Muxy |
| [herdr](https://github.com/herdrdev/herdr) | Optional external terminal runtime; detection manifests and region extraction adapted (see `crates/muxy-server/src/detection/THIRD_PARTY.md`) | Apache-2.0 | its contributors |

herdr is used as a separate process. Its code is distributed only where it was adapted into
`crates/muxy-server/src/detection/`; the full Apache-2.0 text is in
`crates/muxy-server/src/detection/LICENSE-herdr`.

## Vendored sources (`vendor/`)

| Directory | Upstream | Licence | Notice file |
| --- | --- | --- | --- |
| `vendor/core-text` | https://github.com/servo/core-foundation-rs (The Servo Project Developers) | MIT OR Apache-2.0 | `vendor/core-text/COPYRIGHT`, `LICENSE-MIT`, `LICENSE-APACHE` |
| `vendor/libghostty-vt`, `vendor/libghostty-vt-sys` | https://github.com/uzaaft/libghostty-rs (Uzair Aftab, Leah Amelia Chen) | MIT OR Apache-2.0 | `vendor/libghostty-vt/LICENSE`, `vendor/libghostty-vt-sys/LICENSE` |
| `vendor/xim-ctext` | https://github.com/Riey/xim-rs (Riey) | MIT | `vendor/xim-ctext/LICENSE` |

## Linked, not vendored

- GPUI, from a fork of the Zed editor's UI framework pinned in `Cargo.toml`
  (`https://github.com/muxy-app/gpui`). Apache-2.0.
- libghostty (the terminal emulation library built by `libghostty-vt-sys`, compiled with Zig). MIT.
- Every crate resolved in `Cargo.lock`. `deny.toml` restricts them to MIT, Apache-2.0, BSD,
  ISC, Zlib, Unicode-3.0, MPL-2.0 and BlueOak-1.0.0; run `cargo deny check licenses` for the list.

## Bundled with Linux AppImage builds

- AppImage type2-runtime (MIT, Copyright (c) 2004-23 probonopd): `packaging/linux/AppImage-LICENSE`.
- Shared libraries bundled by linuxdeploy keep their Debian/Ubuntu copyright files under
  `usr/share/doc/<package>/copyright`, and `usr/share/doc/atur-terminal/bundled-sources.json` lists their sources.

## Detection manifests

Agent detection manifests and region extraction: see
`crates/muxy-server/src/detection/THIRD_PARTY.md` (adapted from herdr, Apache-2.0).
