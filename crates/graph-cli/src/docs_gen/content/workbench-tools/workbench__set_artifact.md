---
summary: Puts a whole plan, agent or tool file in the pane as the current draft.
---

## Behavior

- Checks the draft right away and returns any problems. Nothing is written to disk.
- For a plan, omit `yaml` to use the plan `plan__compose_plan` last returned.
- Replacing a plan draft with unsaved changes fails unless `overwrite_draft` is true.
