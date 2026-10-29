# Getting Started

Atur Terminal is part of Atur by Joyo Digitama.
Releases are published at <https://github.com/dzulfikar08/atur-terminal/releases>.

Install: [macOS](#macos-desktop-app) · [Linux](#linux-desktop-app) ·
[CLI and server only](#cli-and-server-only)

After installing: [First steps](#first-steps) · [Upgrading from an earlier install](#upgrading-from-an-earlier-install)

## Requirements

| Part | Runs on |
| --- | --- |
| Desktop app | macOS 14 or newer on Apple Silicon (Intel builds are attached when available); experimental Linux packages target Ubuntu 24.04 on x86_64 |
| `atur-term` CLI and server | macOS 14 or newer, or Linux x86_64 with glibc 2.35 or newer. musl is unsupported. |

The binaries are `atur-terminal` (desktop app), `atur-term` (CLI) and `atur-term-server`; the
environment variables are `ATUR_TERM_*`. The data directory and socket are named after Atur Terminal, so
older clients that look elsewhere do not find its server.

## macOS desktop app

1. From the [latest release](https://github.com/dzulfikar08/atur-terminal/releases/latest),
   download `AturTerminal-<version>-arm64.dmg` for Apple Silicon
   (`AturTerminal-<version>-x86_64.dmg` for Intel, when attached).
2. Drag **Atur Terminal** to `/Applications`.
3. Release builds are Developer ID signed and notarized when the signing secrets are
   configured in CI, and then open normally. If you installed an unsigned build
   (ad-hoc signed, not notarized), Gatekeeper blocks the first launch. Either
   Control-click the app, choose **Open**, then **Open** again, or clear the
   quarantine flag once:

   ```bash
   xattr -dr com.apple.quarantine "/Applications/Atur Terminal.app"
   ```

To use [`atur-term`](../features/atur-term-cli.md) in your shell, choose **Install Command
Line Tool…** from the app menu. It links `~/.local/bin/atur-term`.

In-app updates on macOS need a Developer ID signature. For unsigned builds they are
unavailable: download the newer DMG and replace the app. Running terminals keep
going while you do.

## Linux desktop app

Linux support is **experimental**, targeting Ubuntu 24.04 on x86_64, with glibc
2.39 or newer and a working Vulkan driver. See the
[platform limits and WebKit sandbox setup](../tech/constraints.md#experimental-linux-desktop).

Choose the [quick installer](#quick-installer-with-in-app-updates) for in-app
updates, a [Debian package](#debian-package), or an [AppImage](#appimage).
All three include the desktop app, CLI and server. Use one format to avoid
conflicting CLI commands.

### Quick installer with in-app updates

Install the Ubuntu dependencies once:

```sh
sudo apt update
sudo apt install curl python3 ca-certificates libfreetype6 libfontconfig1 libxcb1 \
  libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libvulkan1 mesa-vulkan-drivers \
  libwebkit2gtk-4.1-0 xdg-utils xdg-desktop-portal bubblewrap xdg-dbus-proxy
```

Install the **latest release** as your normal user, without `sudo`.
The installer verifies the download:

```sh
curl -fsSL --proto '=https' --proto-redir '=https' \
  https://github.com/dzulfikar08/atur-terminal/releases/latest/download/install-desktop-linux.py \
  | python3 - install
```

Open **Atur Terminal** from Applications. The app, CLI and server install into
`~/.local/share/atur-terminal` by default. Add `~/.local/bin` to your shell's
`PATH` to use `atur-term <folder>` from a terminal.

### Debian package

Download `atur-terminal_<version>_amd64.deb` from the
[latest release](https://github.com/dzulfikar08/atur-terminal/releases/latest). In your
download directory, replace the filename below with your download:

```sh
sudo apt install './atur-terminal_<version>_amd64.deb'
```

APT installs the dependencies and adds **Atur Terminal** to Applications, with `atur-term`
and `atur-term-server` in `/usr/bin`. The package is named `atur-terminal`.

### AppImage

Install the [runtime libraries above](#quick-installer-with-in-app-updates), then
download `AturTerminal-<version>-linux-x86_64.AppImage` from the
[latest release](https://github.com/dzulfikar08/atur-terminal/releases/latest). Replace the
filename below with your download and run it without `sudo`:

```sh
chmod +x './AturTerminal-<version>-linux-x86_64.AppImage'
'./AturTerminal-<version>-linux-x86_64.AppImage'
```

The first launch adds **Atur Terminal** to Applications. AppImage uses **system WebKitGTK**.
If FUSE is unavailable, append `--appimage-extract-and-run` when launching it.

Atur Terminal keeps an unpacked copy in
`${XDG_DATA_HOME:-$HOME/.local/share}/atur-terminal-appimage/<bundle-id>` so terminals can
keep running after the window closes. This takes extra disk space. Use
`./AturTerminal-<version>-linux-x86_64.AppImage --cli <arguments>` for the bundled CLI,
or choose **Install Command Line Tool…** in the app to link it into `~/.local/bin`.

### Updates

- **Quick installer:** the app checks the releases of `dzulfikar08/atur-terminal`
  automatically; **Check for Updates** checks now.
- **Debian package:** stop the server and quit Atur Terminal, then download the newer
  `.deb` and install it with `sudo apt install`. There is no APT repository.
- **AppImage:** quit the app, then download and run the newer file. Running
  terminals stay alive. If you linked the CLI, choose **Install Command Line Tool…**
  again from the new app.

Debian packages and AppImages do not use the in-app updater.

### Remove

Stop the local server in **Settings → Server**, then quit Atur Terminal. Stopping the
server ends its running terminals. Removal keeps your settings, projects and
saved session data.

- **Quick installer:** run

  ```sh
  python3 "${XDG_DATA_HOME:-$HOME/.local/share}/atur-terminal/current/install.py" uninstall
  ```

- **Debian package:** run `sudo apt remove atur-terminal`.
- **AppImage:** delete the downloaded file, its matching unpacked copy listed
  above, and any CLI link or Applications entry pointing into that copy. The entry
  is in `${XDG_DATA_HOME:-$HOME/.local/share}/applications/com.joyodigitama.atur-terminal.desktop`.
  Do not delete a copy still used by another running app or server.

## CLI and server only

For a Linux computer or Mac without the desktop app. This installs `atur-term` and
`atur-term-server`.

Set `VERSION` to the newest version on the
[releases page](https://github.com/dzulfikar08/atur-terminal/releases/latest) and `TAG` to its
release tag (for example version `2.2.0`, tag `atur-v2.2.0-1`):

```bash
VERSION=2.2.0
TAG=atur-v2.2.0-1
curl -fsSL "https://github.com/dzulfikar08/atur-terminal/releases/download/$TAG/install-atur-terminal.sh" \
  | sh -s -- --version "$VERSION" --tag "$TAG"
export PATH="$HOME/.local/bin:$PATH"
```

The script picks the build for your OS and architecture, verifies its checksum,
and installs both commands into `~/.local/bin`. Add the `export` line to your
shell's startup file to keep them on `PATH`. Without `--tag` it uses `atur-v$VERSION`.

- **Update:** run it again with the new version, tag and `--replace`.
- **Another folder:** add `--install-dir PATH`.

### Check it

Run `atur-term --version` and `atur-term-server --version`, then `atur-term` to open the
[terminal UI](../features/terminal-ui.md). Updating them never restarts a
running [server](../features/server.md#lifetime); stop it to switch to the new
version.

## Upgrading from an earlier install

On first start, if the Atur Terminal data directory does not exist yet and the matching one from an earlier install
does, Atur Terminal copies `server.toml`, `catalog.json`, `remote.json`, `backend.toml` and
`herdr.json` from it. The old directory is left untouched, and sessions are not carried over.
Nothing is copied when `ATUR_TERM_DIR` is set. Unless you already have
projects or workspaces, the first launch imports a 1.x setup: supported
settings, shortcuts, local projects, workspaces, terminal tabs, the default shell,
and extensions. Projects, workspaces, and extensions that could not be imported are
listed, and your 1.x data stays where it was. To import again later, open
**Settings → Backup & Restore** and choose **Import installed 1.x**; it adds to what
you already have.

Some 1.x features work differently or are not in 2.x yet. Most extensions keep
working; see [Coming from 1.x](../extensions/get-started.md#coming-from-1x).

## First steps

1. **Add a project.** Choose **Add Project** at the bottom of the sidebar, or
   press `Cmd+O`, and pick a folder.
2. **Open a terminal.** `Cmd+T` opens a tab in the project. `Cmd+N` opens one
   in Home.
3. **Split it.** `Cmd+D` splits right, `Cmd+Shift+D` splits down.
4. **Quit freely.** Terminals keep running in the background. Reopen Atur Terminal and
   everything is where you left it.

## Next

- [How Atur Terminal works](../features/server.md): the server, sessions, and history.
- [Projects](../features/projects.md), [Terminal](../features/terminal.md), and
  [Keyboard shortcuts](keyboard-shortcuts.md).
- [Remote servers](../features/remote-servers.md) and
  [Mobile](../features/mobile.md).
- [Settings](settings.md) and [Troubleshooting](troubleshooting.md).
