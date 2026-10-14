---
summary: Syncs a plan's inline review threads on a pull request.
---

## Behavior

- Posts each new finding as an inline comment that `gh_pr_review_threads` can read back.
- Applies a transition to each existing finding: `resolve` and `decline` reply and resolve the thread; `none` leaves it alone.
- Never deletes open threads or human replies.
- Has side effects.
