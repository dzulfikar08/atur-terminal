# Tabs

A tab type is a web page that opens as a tab, next to terminal tabs. It can be
split, moved, and pinned like any tab, and is restored on launch.

```json
"tabTypes": [
  { "id": "viewer", "title": "Viewer", "entry": "viewer/index.html", "defaultData": { "mode": "list" } }
]
```

| Field | Notes |
| --- | --- |
| `id` | Unique within the extension. |
| `title` | The tab's default title. |
| `entry` | HTML page in the extension folder. |
| `defaultData` | Data used when the tab is opened without any. |

Open a tab type from a [command](commands.md) with the `openTab` action, or
from code.

## Opening tabs

`tabs.open` needs `tabs:write` and returns the new tab's ID.

```js
// a page from this or another extension
await atur.tabs.open({
  kind: "extensionWebView",
  extension: { id: "hello", tabType: "viewer", data: { mode: "grid" }, singleton: true },
});

// a terminal in the current project, optionally in a subfolder or running a command
await atur.tabs.open({ kind: "terminal", directory: "src", command: "npm test" });

// a browser tab; drive it with atur.browser
await atur.tabs.open({ kind: "browser", url: "localhost:3000" });
```

- `singleton: true` focuses an existing tab of that type instead of opening
  another.
- Running a command, or opening another extension's tab, asks the user first.
- A terminal's `directory` must be a folder inside the current project, and
  works for local projects only.

## Page data

Each tab has its own `data`, saved with the tab.

```js
render(atur.data);
atur.onDataChange(render);
atur.tabs.setTitle("Report");
atur.tabs.setIcon({ symbol: "doc.text" });   // or { svg: "..." }, or null
```

## Theme

Pages get the app's colors as CSS variables, updated live:

```css
body {
  background: var(--atur-background);
  color: var(--atur-foreground);
  border-color: var(--atur-border);
}
```

Variables: `--atur-background`, `--atur-foreground`, `--atur-foreground-muted`,
`--atur-surface`, `--atur-surface-solid`, `--atur-border`, `--atur-hover`,
`--atur-accent`, `--atur-accent-foreground`, `--atur-accent-soft`,
`--atur-diff-add`, `--atur-diff-remove`, `--atur-diff-hunk`, and
`--atur-topbar-height`. The same values are in `atur.theme`, with
`colorScheme` set to `light` or `dark`. `atur.onThemeChange(fn)` reports
changes.

## File openers

A file opener lets an extension open files that users `Cmd`-click in a
terminal, in one of its tab types.

```json
"fileOpeners": [
  { "id": "markdown", "title": "Markdown preview", "tabType": "viewer", "patterns": ["*.md"] }
]
```

| Field | Notes |
| --- | --- |
| `tabType` | One of the extension's tab types. |
| `patterns` | Globs matched against the project-relative path, case-insensitive. Default `["*"]`. |
| `singleton` | Reuse one tab. Default `true`. |

Users pick it in **Settings → General → Open files with**; extensions never
open files unasked. The tab receives `data` like
`{ "filePath": "docs/README.md", "line": 12, "column": 3, "source": "terminal" }`,
with `filePath` relative to the project.

## Pages

- Pages load only from the extension's own folder. Navigating elsewhere is
  blocked.
- Pages stay alive while hidden.
- `atur.lifecycle.onBeforeClose(fn)` can keep a tab open, for example to save
  first. See [API reference](api.md#page-only).
