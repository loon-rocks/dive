---
summary: Finds the catalog tools that fit a task, most likely first.
fields:
  mode:
    type: string
    description: How tools were ranked, `decision` (a decision model chose) or `keyword` (keyword match).
  tools:
    type: object[]
    description: The tools that fit, most likely first, with their scores and, when `schemas` is on, their input schemas and output shapes.
  always:
    type: string[]
    description: Tools that are always offered, from `[tools].always_loaded`. They're never ranked.
---

## How it works

- With a decision model configured, the model chooses among every tool, and its probabilities rank them.
- Without one, keywords rank them.
- Tools below `min_probability` are dropped, so the list can be empty.

`compose_plan` and the drafter agents use this plan to find tools. Every agent also calls it to load the tools on its list: called from an agent, it searches only the tools that agent may call, and the tools it returns are loaded for the rest of the thread.
