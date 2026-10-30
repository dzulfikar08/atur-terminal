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

## Screen mirroring

The Screen view (`crates/atur-screen`, the screen channel in `muxy-protocol`, `muxy-server` and
`muxy-client`) speaks the RFB protocol itself; no VNC library is vendored. The RFB specification
is public.

Crates it adds to the build, all already in `Cargo.lock` through other dependencies:

| Crate | Used for | Licence |
| --- | --- | --- |
| `flate2` (with `miniz_oxide`) | The zlib streams of ZRLE and of the extended clipboard | MIT OR Apache-2.0 |
| `core-video` 0.4.3 (macOS only) | The decoded picture handed to GPUI as a CoreVideo buffer | MIT OR Apache-2.0 |
| `core-foundation` 0.10 (macOS only) | Core Foundation objects around VideoToolbox | MIT OR Apache-2.0 |

Programs it runs, none of which is bundled or linked:

| Program | Where | Licence | Role |
| --- | --- | --- | --- |
| [wayvnc](https://github.com/any1/wayvnc) and its library neatvnc | On the server, installed by the user | ISC | Captures the Wayland desktop and encodes it; started and stopped by the server |
| [ffmpeg](https://ffmpeg.org) | On a Linux client, installed by the user | LGPL-2.1+ (GPL for some builds) | Decodes H.264, on the GPU through VA-API when present; run as a separate process |
| VideoToolbox (Apple system framework) | On macOS | Part of the operating system | Decodes H.264 |

H.264 decoding is therefore never shipped in Atur Terminal: macOS uses the operating system's
licensed decoder, Linux uses the user's own `ffmpeg`, and a client with neither asks the server
for ZRLE instead. A software decoder such as Cisco's OpenH264 was deliberately not linked: its
BSD licence covers the code, but its patent licence only covers the binaries Cisco builds and
distributes itself, which this project does not.

## Bundled with Linux AppImage builds

- AppImage type2-runtime (MIT, Copyright (c) 2004-23 probonopd): `packaging/linux/AppImage-LICENSE`.
- Shared libraries bundled by linuxdeploy keep their Debian/Ubuntu copyright files under
  `usr/share/doc/<package>/copyright`, and `usr/share/doc/atur-terminal/bundled-sources.json` lists their sources.

## Detection manifests

Agent detection manifests and region extraction: see
`crates/muxy-server/src/detection/THIRD_PARTY.md` (adapted from herdr, Apache-2.0).
