---
summary: Returns a release's tag, title, notes and date, plus the previous release's tag.
---

## Behavior

- Omit `tag` for the latest release.
- `previous_tag` is the base ref for a `git_log` or `git_diff` over what shipped.
- `previous_tag` is null on the first release, so check it before diffing.
