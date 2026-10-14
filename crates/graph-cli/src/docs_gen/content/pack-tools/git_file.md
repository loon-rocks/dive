---
summary: Returns a file's contents at a git ref.
---

## Behavior

- Reads the file as it exists at `ref`, not the working copy.
- Truncates to a byte budget. A truncated file ends with a `[file truncated: …]` line.
