# Server

`atur-term-server` runs every terminal. The desktop app, the terminal UI, the CLI,
and phones are all clients of it. That is why terminals keep running after you
quit an app, and why several apps can show the same terminal.

```mermaid
flowchart LR
    DESKTOP["Desktop app"] <--> SERVER["atur-term-server"]
    TUI["atur-term terminal UI"] <--> SERVER
    CLI["atur-term CLI"] <--> SERVER
    PHONE["Phone"] <--> SERVER
```

## Lifetime

- Apps and CLI commands start the server when they need it. You never need to
  start it yourself.
- It keeps running until you stop it, even with no app open.
- Stopping or restarting it ends every terminal. Saved screens and history are
  kept. Use the status bar, **Settings → Server**, or
  `atur-term server stop --force`.
- Atur Terminal never restarts it on its own while terminals are running. After an
  update, the desktop app replaces this computer's server once no terminals are
  running.

## Sessions

- A session is one running terminal. It belongs to the project it was created
  in, wherever you `cd` later.
- It ends when its program exits, when someone ends it, or when the server
  stops. Closing the last pane that shows it ends it too.
- Quitting an app, detaching, a crash, or sleep never ends it.
- Any number of apps can show and type into a session at once. They share one
  size.
- **Existing Terminals** lists a project's sessions that this app isn't
  showing, and which app owns each one, so you can open them here too.

## History

- The server saves each terminal's screen and scrollback to disk, even when no
  app is watching. It is still there after the program ends or the server
  restarts.
- Scrollback size is a byte budget per terminal, 16 MiB by default. Older
  history loads as you scroll.
- Ended sessions and their saved output are removed after 7 days.

## Settings

Server settings apply to everyone using that server. Change them in
**Settings → Server**, with `atur-term settings set`, or in `server.toml`:

| Setting | `server.toml` key | Default |
| --- | --- | --- |
| Default shell | `default_shell` | `$SHELL`, else `/bin/zsh` on macOS and `/bin/sh` on Linux |
| History budget per session | `history_budget_bytes` | `16777216` (16 MiB) |
| Shell integration | `shell_integration` | `true` |

Shells start as login shells. Changes apply to new terminals. A new history
budget applies to saved history after the server restarts.

## Shell integration

Shell integration lets Atur Terminal track each terminal's folder, jump between prompts,
and select a command's output. It loads automatically for zsh and fish, without
editing your startup files.

Bash is opt-in. Add this to the file your Bash profile loads for interactive
shells:

```bash
if [[ ${ATUR_TERM_SHELL_INTEGRATION:-0} == 1 ]]; then
    source "$ATUR_TERM_SHELL_INTEGRATION_DIR/atur-term.bash"
fi
```

Every terminal gets `TERM=xterm-256color`, `COLORTERM=truecolor`,
`TERM_PROGRAM=atur-terminal`, `ATUR_TERM_SERVER_ID`, and `ATUR_TERM_SESSION_ID`.

## Files

Everything lives in one profile folder:

| Platform | Stable | Beta |
| --- | --- | --- |
| macOS | `~/Library/Application Support/Atur Terminal` | `~/Library/Application Support/Atur Terminal Beta` |
| Linux | `$XDG_STATE_HOME/atur-terminal`, or `~/.local/state/atur-terminal` | `$XDG_STATE_HOME/atur-terminal-beta`, or `~/.local/state/atur-terminal-beta` |

| File | What it holds |
| --- | --- |
| `server.toml` | Server settings |
| `server.log` | Server log |
| `server.sock` | The socket apps connect to |
| `sessions/` | Projects, sessions, and saved terminal output |
| `remote.json` | Mobile access settings and paired phones |
| `settings.toml`, `terminal.toml`, `themes/` | Desktop app settings, see [Settings](../user-guide/settings.md) |
| `tui-state.json` | Terminal UI layouts |

On first start, if the profile folder does not exist and the matching Muxy folder does (`muxy`,
`muxy-beta`, `muxy-dev` on Linux; `Muxy 2`, `Muxy Beta`, `Muxy Dev` on macOS), `server.toml`,
`catalog.json`, `remote.json`, `backend.toml` and `herdr.json` are copied into it. The Muxy folder is
never changed, and nothing is copied when `ATUR_TERM_DIR` is set.

Set `ATUR_TERM_DIR` to use a different profile folder. Development builds use
`Atur Terminal Dev` (`atur-terminal-dev` on Linux), so they never touch your real data.
