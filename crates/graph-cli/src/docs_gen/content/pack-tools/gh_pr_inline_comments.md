---
summary: Posts inline review comments on a pull request's diff.
---

## Behavior

- Posts one comment per entry that has a line anchor.
- Entries without an anchor, or with one GitHub rejects, come back unposted.
- Deletes earlier comments with the same marker first, so reruns replace them.
- Has side effects.
