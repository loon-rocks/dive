---
summary: Adds one top-level step to the plan draft.
---

- Appends the step, or places it `before` or `after` an existing step id.
- Refused only if it adds a new validation problem; existing problems are reported.
- Steps inside `route`, `map` and `reduce` bodies are edited through their control step with `workbench__update_step`.
