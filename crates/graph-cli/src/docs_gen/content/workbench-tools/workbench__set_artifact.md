---
summary: Puts a whole agent, tool or plan file in the network draft as the draft of the sketch node it names.
---

## Behavior

- Checks the draft right away and returns any problems. Nodes of the sketch that aren't drafted yet count as existing. Nothing is written to disk.
- Once there's a sketch, a file whose name isn't a node of it is refused.
- For a plan, omit `yaml` to use the plan `plan__compose_plan` last returned. With no network in progress, a plan goes to the plan pane, and replacing one with unsaved changes fails unless `overwrite_draft` is true.
