# Atur Terminal CLI

The `atur-term` command talks to the [server](server.md) from a shell. It manages
projects, terminal sessions, worktrees, server settings, and phone access. No
desktop app is needed, so it works on Linux and over SSH too.

Commands work with what the server owns: projects and sessions. Tabs, panes, and
workspaces belong to each app, so there are no commands for them. Browser tabs
are the exception: they live in the desktop app, and `atur-term browser` talks to
the app on this computer. The [terminal UI](terminal-ui.md) is an app of its
own, with its own layout.

## Install

- **With the desktop app:** choose **Install Command Line Tool…** from the app
  menu. It links `~/.local/bin/atur-term` to the copy inside the app.
- **On its own:** see [Getting started](../user-guide/getting-started.md#cli-and-server-only).

Make sure `~/.local/bin` is on your `PATH`.

## Basics

```bash
atur-term                 # open the terminal UI
atur-term .               # open this folder in the desktop app or the terminal UI
atur-term project list    # list projects
atur-term --help          # all commands; atur-term <command> --help for details
```

- Commands start the server they use if it isn't running, including a remote
  one with `--host`. `atur-term server status` and `atur-term server stop` never start
  it.
- Management commands take `--json` for machine-readable output, except
  `atur-term mobile`.
- Ending or discarding a session and deleting a project or worktree need
  `--yes`.
- A project can be named by its ID, its name if unique, or its folder.
- `--host <destination>` runs the command against the server on another
  computer. It must come first: `atur-term --host devbox project list`. See
  [Remote servers](remote-servers.md).

## Commands

| Command | What it does |
| --- | --- |
| `atur-term` | Opens the [terminal UI](terminal-ui.md). |
| `atur-term <folder>` | Opens the folder as a project in the desktop app. On Linux, over SSH, or without the desktop app, opens it in the terminal UI. |
| `atur-term server start\|status\|stop` | Starts, inspects, or stops the server. `stop` refuses while terminals run; add `--force` to end them. |
| `atur-term project list\|add\|rename\|set-color\|set-icon\|delete` | Manages projects. Deleting ends its terminals but keeps files on disk. |
| `atur-term session list\|create\|send\|send-keys\|read-screen\|history\|search\|wait\|end\|discard` | Creates terminals, sends input, reads output, and waits for text or exit. |
| `atur-term worktree list\|create\|checkout-pr\|register\|remove` | Manages Git worktree projects, including checking out pull requests. |
| `atur-term settings get\|set` | Reads or changes [server settings](server.md#settings). |
| `atur-term activity list\|ack` | Reads or clears [AI agent](ai-agents.md) activity. |
| `atur-term browser open\|list\|read\|click\|…` | Drives browser tabs in the desktop app on this computer. See [Browser control](#browser-control). |
| `atur-term exec <project> -- <program> [args]` | Runs a program in the project folder, without a shell. |
| `atur-term git`, `atur-term files` | Run raw server Git and file actions as JSON. Advanced; the JSON shape follows the protocol and may change. |
| `atur-term mobile` | Turns on phone access and pairs phones. See [Mobile](mobile.md). |
| `atur-term install-skills` | Installs the Atur Terminal CLI skill for the AI agents found in your home folder. `--dir <path>` adds another skills folder. |
| `atur-term stdio` | Joins stdin and stdout to the server. Used by `--host` over SSH; you don't run it yourself. |

## Sessions from a script

```bash
id=$(atur-term session create my-app)
atur-term session send "$id" "npm test"
atur-term session send-keys "$id" Enter
atur-term session read-screen "$id" --lines 20
atur-term session end "$id" --yes
```

- `send` types text without pressing Return. `send-keys` sends `Enter`, `Tab`,
  `Escape`, `Backspace`, `Ctrl+C`, `Ctrl+D`, or `Ctrl+Z`.
- `read-screen` returns the last visible rows. `history` and `search` page
  through scrollback as JSON.
- `--saved` reads a session's last saved screen, even after it ended.
- `end` stops the program and keeps its saved output. `discard` also deletes
  the output.
- Sessions created here outlive the command and appear in every app under
  **Existing Terminals**.
- Inside an Atur Terminal terminal, session commands default to that terminal, and
  `create` to its project.
- `create` can take a `--directory` and a command to run after `--`.
- `wait --text <text>` waits for text on the screen, and `wait --exit` for the
  shell to exit. Both give up after 30 seconds by default.

## Browser control

```bash
tab=$(atur-term browser open localhost:3000)
atur-term browser wait-for-navigation "$tab"
atur-term browser fill "$tab" "#email" "me@example.com"
atur-term browser click "$tab" "button[type=submit]"
atur-term browser wait "$tab" --text "Welcome"
atur-term browser read "$tab"
atur-term browser screenshot "$tab" | base64 -D > page.png
atur-term browser close "$tab"
```

- Browser commands need the desktop app and don't take `--host`. `open` starts
  the app if it isn't running.
- `open` prints the new tab's ID. Pass it to the other commands.
- `snapshot` lists a page's interactive elements, each with a selector for
  `click`, `fill`, or `type`.
- `atur-term browser --help` lists every command.

## Exit codes

`0` on success. On any error, `atur-term` prints `atur-term: <message>` to stderr and exits
with `1`, including when a program run with `atur-term exec` fails.

## Profiles

`ATUR_TERM_DIR` points the CLI, and the server it starts, at another profile folder.
Use it to run a separate, throwaway server. See [Server](server.md#files).
