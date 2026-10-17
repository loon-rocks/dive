---
summary: Runs a tool draft, or the plan in the plan pane.
---

## Behavior

- A tool runs once on a sample input, after you approve it. Name it when several tool drafts are open.
- A tool whose `env` needs a secret that isn't set doesn't run: the result is `status: missing_secrets`, as with `workbench__test_network`.
- A plan runs with its steps streaming to the pane. `gated` or `breakpoints` make it a debug run that pauses for you.
- Agents and network plans are tried with `workbench__test_network`.
