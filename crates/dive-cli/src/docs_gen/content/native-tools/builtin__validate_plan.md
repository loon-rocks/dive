---
summary: Validates a plan's structure, templates and tool calls against the catalog.
---

- Pass the whole plan as `plan`, or just its `steps`.
- Returns every problem, plus the called tools that aren't marked read-only.
