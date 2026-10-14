---
summary: Lists the files changed between two git refs, with per-file status and line counts.
---

## Behavior

- Uses the merge-base diff, filtered to a path prefix.
- Each change has a `status`: `added`, `modified`, `deleted`, `renamed`, `copied` or `type_changed`.
- Deleted files don't exist at the head ref. Filter them out before reading files with `git_file`.
- No changes returns `count: 0`, not an error.
