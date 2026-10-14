---
summary: Returns the unified diff between two git refs.
---

## Behavior

- Uses the merge-base diff (`base...head`), optionally scoped to paths.
- Truncates to a byte budget. A truncated diff ends with a `[diff truncated: …]` line.
