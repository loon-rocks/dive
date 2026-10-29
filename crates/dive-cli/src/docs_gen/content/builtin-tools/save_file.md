---
summary: Stores text or JSON you already have as a file and returns its ref, with no model call.
---

## Behavior

- A string `content` is written as it is. An object or list is written as pretty JSON.
- `name` is a file name, not a path, and its extension sets the file's type: `snapshot.json` is `application/json`, `styles.css` is `text/css`. Any lowercase extension of up to 16 letters and digits is kept, so a type Dive doesn't know is stored as `application/octet-stream` with its own extension.
- A name without an extension takes one from `type` when given, else `.txt` for a string and `.json` for anything else.
- Returns `{name, ref, mime, bytes}`, and lists the file under `files` like any tool that returns one.

## When to use it

- Keep data a plan or agent already has, such as a backup of a setting before changing it, as a ref to pass on or read back later with [`builtin__read_file`](/tools/reference/builtin/read_file).
- Hand text to a tool that takes a file: pass the ref to an input declared `format: blob`.
