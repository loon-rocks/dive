---
summary: Lists the commits between two git refs, newest first.
---

## Behavior

- Covers `base..head`: what landed since `base`.
- Excludes merge commits unless you set `include_merges`.
- Parses the pull request number from a trailing `(#123)` in each subject.
- An empty range returns `count: 0`, not an error.
