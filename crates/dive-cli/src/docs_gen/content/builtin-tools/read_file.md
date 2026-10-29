---
summary: Reads a text file's content from its ref, with no model call.
---

## Behavior

- Takes a ref to a text file: JSON, CSS, HTML, Markdown, CSV, YAML, XML, plain text and other `text/*` types.
- Returns `{ref, mime, bytes, text}`, plus `json` when the whole text parses as JSON.
- A file longer than `max_bytes` (default 1 MB) is cut on a character boundary and returned with `truncated: true`.
- Images, PDFs and other binary files are refused. Models see those when their ref appears in what they read; tools get them through a `format: blob` input.

## When to use it

- Read back a file saved with [`builtin__save_file`](/tools/reference/builtin/save_file), such as a backup to restore: `{{read.json.customCss}}`.
- Use a text file a tool returned in a later step's input.
