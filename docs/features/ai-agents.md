# AI Agents

Atur Terminal recognizes AI coding agents running in its terminals and shows whether
each one is working, waiting for you, or done. It needs no setup, hooks, or
plugins, and works while no app is open.

## Supported agents

Antigravity, Claude Code, Codex, Copilot, Cursor, Droid, Grok, Kiro, Oh My Pi,
OpenCode, Pi, and Xal.

The server spots them from the running program and what's on screen.
Recognition is best effort.

Oh My Pi detection reads its OSC terminal-title run state: a spinner while
working, `!` when waiting, and `>` when idle. It also recognizes optional OSC
progress reports, `⎋` working messages, and the older `Working… [esc]` spinner.
In `omp --print` sessions, a final non-empty
`Working...` line signals work; once output follows it, that progress line
no longer keeps the session busy. Detection uses terminal escape sequences
and screen text, so disabling or customizing these signals can affect recognition.

For foreground `omp` and `oh-my-pi` processes, automatic terminal titles omit
the leading `π ` prefix. The provider icon remains visible, and custom tab
titles are unchanged.

## States

| State | Meaning |
| --- | --- |
| Working | The agent is busy. |
| Waiting | The agent needs your input. |
| Idle | The agent is done or not doing anything. |

Tabs, worktrees, and projects show the agent's icon and state. When several
agents run, "waiting" wins.

## Notifications

The desktop app sends a macOS notification when an agent needs your attention
or finishes working.

- No notification is shown for the pane you are looking at, or for detached
  terminals.
- Looking at the terminal clears the alert in every app.
- Clicking a notification jumps to its pane.
- Only one app notifies, so alerts are never doubled.
- Turn notifications off in **System Settings → Notifications → Atur Terminal**.

Alerts live in memory on the server, at most one per terminal. Restarting the
server clears them.

## Agents panel

`Cmd+Opt+A` shows the agents panel, focuses it, and hides it once it has
focus. Use the arrow keys and Return to open a row.

- It sits at the bottom of the expanded sidebar; drag its top edge to make it
  taller or shorter. With the sidebar collapsed it
  opens as a popup that closes when you click elsewhere. Once you have shown
  the panel, the collapsed sidebar keeps a button for it until you hide it.
- **Move to Right** docks it in the right panel, even when the left sidebar is
  collapsed. Drag its left edge to resize it; Atur Terminal remembers the side and width.
  It shares this area with extension panels and the panel-mode composer: opening
  one replaces the other. Use `Cmd+Opt+A` to bring Agents back. **Move to Left
  Sidebar** returns it to the sidebar.
- **Detach** floats it over the workspace. Drag its header to move it and its
  edges or corner to resize it; Atur Terminal remembers where. **Reattach to Sidebar** or
  **Reattach to Right Panel** docks it on its last side. The other side's move
  button is also available while floating.
- **Settings → Appearance → Agents panel rows** lists each agent, grouped as
  needs input, working, completed, and idle, where Return opens its pane; or
  each project and worktree running agents, with counts, where Return opens
  the worktree.

## Agents Focused sidebar

Choose **Agents Focused** from the sidebar layout menu to list only the tabs
that run an agent.

## From a shell and extensions

- `atur-term activity list --json` prints agent status and unread alerts as JSON.
  `atur-term activity ack <event-id>` clears alerts.
- Extensions get the `agent.status` event and `atur-term.agents.list()`. See
  [Events](../extensions/events.md).
