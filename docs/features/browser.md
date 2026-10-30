# Browser

The desktop app has a built-in browser. A browser is a pane like a terminal,
so it opens in a tab or a split, and its page keeps running while hidden.

## Open a page

- **File → New Browser Tab** (`Cmd+Opt+B` on macOS, `Ctrl+Alt+B` on Linux), the globe button in the tab
  strip, or the command palette.
- `atur-term browser open <url>` from a terminal. See [Atur Terminal CLI](atur-term-cli.md#browser-control).
- Command-click a link in a terminal, when **Settings → Browser → Open terminal
  links in the browser** is on.
- A page that opens a new window gets a new tab instead.

Type an address or a search in the address bar. Matching history shows below
it. Links to other apps, such as `mailto:`, open in those apps.

## In a page

| Action | macOS | Linux |
| --- | --- | --- |
| Address bar | `Cmd+L` | `Ctrl+L` |
| Reload | `Cmd+R` | `Ctrl+R` |
| Back / forward | `Cmd+[` / `Cmd+]` | `Alt+Left` / `Alt+Right` |
| Find on page | `Cmd+F` | `Ctrl+Shift+F` |
| Zoom in / out / reset | `Cmd+=` / `Cmd+-` / `Cmd+0` | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |
| Copy the page's address | `Cmd+Shift+C` | `Ctrl+Shift+C` |
| Web Inspector | `Cmd+Opt+I` | `Ctrl+Shift+I` |

The toolbar also opens the page in another browser and switches the tab's
profile.

## Profiles

Each profile keeps its own cookies, site data, history, and logins. New tabs use
the default profile. Manage profiles in **Settings → Browser**, where you can
also import history, bookmarks, and cookies from another browser on this
computer.

## Saved and not saved

A browser tab remembers its address and profile across restarts. Its title,
zoom, and back/forward history start fresh.

## Drive your own browser (Helium, Chromium, Chrome)

`atur-term browser` can drive the browser you already use instead of the
built-in one, so agents work in pages where you are signed in. Pick it per
command with `--engine helium` (also `chromium`, `chrome`), per shell with
`ATUR_TERM_BROWSER_ENGINE=helium`, or for good in **Settings → Browser → Browser
for agents**. The verbs, options, and replies are the same as for the built-in
browser; `--engine builtin` goes back.

```bash
tab=$(atur-term browser --engine helium open localhost:3000)
atur-term browser --engine helium fill "$tab" "#email" "me@example.com"
atur-term browser --engine helium read "$tab"
```

### How it works

The browser is driven over the Chrome DevTools Protocol on its remote debugging
port, on `127.0.0.1` only. Atur Terminal never copies or decrypts cookies; the
agent sees what a page shows, as you would.

- **Helium isn't running:** `atur-term` starts it on your normal profile with a
  debugging port on a free loopback port.
- **Helium runs with debugging on:** `atur-term` attaches to it.
- **Helium runs without debugging:** a browser can't be attached to afterwards,
  and a second copy can't share your profile. `atur-term` stops and says so;
  it never restarts the browser silently. Run the command again with
  `--restart-browser`, or answer `y` when asked at a terminal, and it quits
  Helium gracefully and starts it again with debugging and `--restore-last-session`.
  Or do it once yourself, as below.

### Turn debugging on in Helium

One-time, for the running browser: quit Helium, then start it with

```bash
helium-browser --remote-debugging-port=9222 --remote-debugging-address=127.0.0.1
```

To make it permanent, use either of these. The flags file applies however
Helium starts (launcher, terminal, links from other apps):

```bash
mkdir -p ~/.config
printf '%s\n' '--remote-debugging-port=9222' '--remote-debugging-address=127.0.0.1' \
  >> ~/.config/helium-browser-flags.conf
```

Or override the desktop entry for your user only:

```bash
mkdir -p ~/.local/share/applications
sed 's|^Exec=helium-browser |Exec=helium-browser --remote-debugging-port=9222 --remote-debugging-address=127.0.0.1 |' \
  /usr/share/applications/helium.desktop > ~/.local/share/applications/helium.desktop
```

Restart Helium once afterwards. `atur-term` finds the port from the browser's
command line, so no further configuration is needed. On macOS, pass the same
two flags with `open -a Helium --args …` or `--restart-browser`.

### What agents can touch

- Agent tabs open in a window of their own, so you can see what agents do.
  (Browsers offer no way to name a tab group from outside, so the window is
  the marker.) While an agent is working, the status bar shows
  **Agent driving Helium**.
- Agents drive only tabs they opened. `list` shows those, and a command on
  another tab fails. `--any-tab` lifts this for one command, to work in a tab
  you opened yourself.
- `cookies clear` clears only the current page's cookies here, because the
  browser holds all your logins.
- The debugging port is bound to loopback and `atur-term` refuses any other
  address. While it is open, any process on this computer can use it to control
  the browser, including your signed-in sessions. Leave it on only when you
  accept that, or use the one-time start instead of the permanent flags.

### Settings and environment

| Setting / variable | Meaning |
| --- | --- |
| Settings → Browser → Browser for agents, `[browser] engine` | `builtin`, `helium`, `chromium` or `chrome` |
| Browser path, `[browser] engine_path` | The executable, when it isn't found automatically |
| `ATUR_TERM_BROWSER_ENGINE` | Overrides the setting; `--engine` overrides both |
| `ATUR_TERM_BROWSER_PATH` | Executable to start |
| `ATUR_TERM_BROWSER_USER_DATA_DIR` | Profile folder, if not the browser's normal one |
| `ATUR_TERM_BROWSER_ARGS` | Extra flags for a browser that `atur-term` starts |
| `ATUR_TERM_BROWSER_ENDPOINT` | Attach to `127.0.0.1:PORT` as is |

`open --split`, `--project`, `--worktree` and `--profile` have no meaning in an
external browser and are ignored. Chrome 136 and later ignore the debugging port
for their default profile folder; Helium and Chromium builds without that
restriction work on the normal profile, and for Chrome set
`ATUR_TERM_BROWSER_USER_DATA_DIR` to another folder.

### omp's browser tool with the same Helium

omp's `browser` tool can attach to the same Helium over the same port. With
debugging on at port 9222, set in omp's config (`~/.omp/agent/config.yml`):

```yaml
browser:
  cdpUrl: http://127.0.0.1:9222
  relay: false   # the relay takes precedence over cdpUrl when on
```

or per call: `browser.open({ name: "main", url, app: { cdp_url: "http://127.0.0.1:9222", relay: false } })`.
To let omp start Helium itself: `app: { path: "/usr/bin/helium-browser", args: ["--remote-debugging-port=9222", "--remote-debugging-address=127.0.0.1", "--user-data-dir=<your Helium profile folder>"] }`.
This only documents the options; nothing in omp's configuration is changed by Atur Terminal.

## Turning it off

Turn off **Settings → Browser → Enable built-in browser** to disable browser
tabs and `atur-term browser`. Terminal links then open in your system browser.

## Linux

The browser uses WebKitGTK 4.1, version 2.42 or newer. It shares the Linux
extension renderer's [input and clipboard limits](../tech/constraints.md#experimental-linux-desktop).
Navigation, profiles, history, find, Web Inspector and CLI automation work through
this renderer, including screenshots of hidden tabs. The app still starts without
WebKitGTK installed; opening a browser reports the missing engine.
Right-click a page and choose **Web Inspector** to open developer tools, or use
`Ctrl+Shift+I`. Pages with their own context menu retain it.

Cookie import from Firefox is available on Linux. Importing encrypted Chromium
cookies requires macOS Keychain and is unavailable on Linux. Website file pickers,
JavaScript dialogs, downloads, camera, microphone and location requests are not
supported by the Linux host.
