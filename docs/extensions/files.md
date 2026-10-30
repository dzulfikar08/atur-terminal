# Files

Read and change files in a project. Calls run on the project's server, so they
work the same for local and remote projects. Available on pages and in scripts.

Paths are relative to the project folder. A tab works on its own project;
other views work on the current project. Pass `{ project }` to target another
project or worktree by ID.

| Call | Needs | Returns |
| --- | --- | --- |
| `files.list(path)` | `files:read` | `[{ name, path, isDirectory, isIgnored }]` |
| `files.read(path)` | `files:read` | `{ path, content, size }` |
| `files.readBytes(path)` | `files:read` | `{ path, base64, size }` |
| `files.stat(path)` | `files:read` | `{ path, name, isDirectory, size }` |
| `files.write(path, contents)` | `files:write` | |
| `files.writeBytes(path, base64)` | `files:write` | `{ path }` |
| `files.mkdir(path)` | `files:write` | |
| `files.rename(path, newName)` | `files:write` | `{ path }` |
| `files.move(paths, into)` | `files:write` | new paths |
| `files.delete(paths)` | `files:write` | |

```js
const entries = await atur.files.list("");
const readme = await atur.files.read("README.md");
await atur.files.write("notes/todo.md", readme.content + "\n- [ ] ship it");

const { base64 } = await atur.files.readBytes("art/logo.png");
await atur.files.writeBytes("art/logo-copy.png", base64);
```

- Writes ask the user first; see [Permissions](permissions.md#runtime-prompts).
- `delete` moves files to the Trash.
- `read` and `write` handle text and are limited to 5 MiB; listings are
  limited to 16,384 entries.
- `readBytes` and `writeBytes` move any file, such as an image or a design
  file, as standard base64. `writeBytes` asks the user like `write`, writes to a
  temporary file beside the target and renames it over the target, so the file
  is replaced whole or not at all. Files may hold up to 64 MiB, sent in pieces.
  A page can post only 1 MiB at a time, so the page API sends a large file in
  pieces of 512 KiB itself and asks once, before the first piece; an error or a
  failed piece removes the temporary file and leaves the target as it was.
  A server older than this version handles at most 5 MiB and answers a larger
  file with an error saying so.
- Subscribe to `file.changed` (with `files:read`) to hear about changes; see
  [Events](events.md).
