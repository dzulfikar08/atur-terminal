<p align="center">
  <img src="packaging/macos/AppIcon.png" alt="Atur Terminal placeholder icon" width="128" height="128">
</p>

<h1 align="center">Atur Terminal</h1>

<p align="center">A terminal multiplexer for developers and AI coding agents: persistent sessions, split panes, projects, remote servers. Part of <strong>Atur</strong> by Joyo Digitama.</p>

Atur Terminal is a fork of [Muxy](https://github.com/muxy-app/muxy) (MIT), built with Rust, GPUI and
libghostty. Terminal sessions can also run on [herdr](https://github.com/herdrdev/herdr) (Apache-2.0),
which it uses as an external process; see [the herdr backend](docs/tech/herdr-backend.md).
It is not affiliated with or endorsed by the Muxy or herdr projects.

## What changed from Muxy, and what did not

**Rebranded (what you see):** the app, window and product name ("Atur Terminal"), the macOS bundle
(`Atur Terminal.app`, id `com.joyodigitama.atur-terminal`), Linux desktop entries and window class
(same id), DMG and AppImage file names (`AturTerminal-*`), menus, dialogs, CLI messages, the
About screen (with the "Based on Muxy" and herdr credits), the Debian package description, and
the update feed (now `dzulfikar08/muxy` releases, not Muxy's).

**Deliberately unchanged (compatibility):** the binary names `muxy`, `muxy-server` and `muxy-app`, crate
names, socket paths and state directories, every `MUXY_*` environment variable, the Muxy client
protocol, the `muxy-desktop` package name, and the built-in "Muxy" theme names. This is so existing
Muxy installs keep their data, the Atur bridge (`BRIDGE_BACKEND=muxy`, the native client) keeps
working, and stock Muxy clients can connect to an Atur Terminal server and the other way round.

**Icon:** Muxy's logos and icons were removed. `packaging/macos/AppIcon.png` is a neutral
placeholder (a terminal prompt on a dark tile), not final artwork.

## Install

Download from the [latest release](https://github.com/dzulfikar08/muxy/releases/latest) and verify
against `SHA256SUMS`.

| Platform | Asset |
| --- | --- |
| macOS 14+, Apple silicon | `AturTerminal-<version>-arm64.dmg` |
| macOS 14+, Intel | `AturTerminal-<version>-x86_64.dmg` (attached when its build succeeds) |
| Linux x86_64, Ubuntu 24.04 | `muxy-desktop_<version>_amd64.deb`, `AturTerminal-<version>-linux-x86_64.AppImage` or `muxy-desktop-<version>-linux-x86_64.tar.gz` |
| CLI and server only, Linux x86_64 (glibc 2.35+) | `muxy-<version>-linux-x86_64.tar.gz` |
| CLI and server only, macOS | `muxy-<version>-macos-<arch>.zip` |

See [Getting started](docs/user-guide/getting-started.md) for the Linux installers, the install script
and upgrading from Muxy.

### macOS: Gatekeeper

The macOS app is **ad-hoc signed and not notarized** (there is no Apple Developer ID in this
project's CI). macOS therefore blocks the first launch of a downloaded copy. After dragging
**Atur Terminal** to `/Applications`, either Control-click it and choose **Open**, or run once:

```bash
xattr -dr com.apple.quarantine "/Applications/Atur Terminal.app"
```

In-app updates on macOS require a Developer ID signature, so they are unavailable; download the new
DMG instead. The Intel build is cross-compiled and only exercised under Rosetta in CI.

## Releases and CI

- `.github/workflows/ci.yml`: Linux (headless and desktop) and macOS checks on pushes to `main`,
  `atur-terminal` and `feat/herdr-backend`, and on pull requests.
- `.github/workflows/atur-release.yml` (with `.github/workflows/build-release.yml`): pushing a tag
  `atur-vX.Y.Z-N` (or running the workflow manually with that tag) builds Linux x86_64 and macOS
  arm64, stamps `X.Y.Z` into the binaries (`N` is the release revision), and attaches the archives,
  installers, `update.json` and `SHA256SUMS` to a GitHub Release. The macOS Intel build is added
  after publishing and never blocks the release.

```bash
git tag atur-v2.2.0-1 && git push origin atur-v2.2.0-1
```

The workflows were adapted from Muxy's. Developer ID signing, notarization, the Sparkle update for
Muxy 1.x, Homebrew and Discord publishing, arm64 Linux and the mobile SDK were removed.

## Build from source

Rust 1.92, Zig 0.15.2 and, on Linux, the packages listed in `.github/workflows/ci.yml`:

```bash
cargo build --release -p muxy-app -p muxy-cli -p muxy-server
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [docs](docs/README.md).

## Licence and credits

- [LICENSE](LICENSE): MIT. Copyright (c) 2026 Muxy, and (c) 2026 Joyo Digitama for the modifications.
- [NOTICE](NOTICE): attributions for Muxy and herdr.
- [THIRD_PARTY.md](THIRD_PARTY.md): vendored and bundled components and their licences. herdr's
  agent-detection manifests are adapted under Apache-2.0; see
  `crates/muxy-server/src/detection/THIRD_PARTY.md` and `LICENSE-herdr`.

Based on Muxy by the Muxy contributors. herdr by its contributors.
