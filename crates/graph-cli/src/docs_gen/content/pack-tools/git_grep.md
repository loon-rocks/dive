---
summary: Searches tracked files at a git ref with a regular expression.
---

## Behavior

- Uses POSIX extended regex, optionally scoped to pathspecs.
- Caps results at `max_matches`; `truncated` flags the cut.
- No match returns `count: 0`, not an error.
