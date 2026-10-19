---
summary: Asks a decision model typed questions about one state, in one call.
---

## Behavior

- Each question is answered independently and in parallel against the same `state`, in one call.
- Question types: `likelihood` (a probability), `choice` (one option) and `score` (a level on a scale).
- A `choice` can hold up to 255 options. To rank a list against a question, use [`builtin__rank`](/tools/reference/builtin/rank), which turns the list into the options.
- The result maps each question name to its answer, such as `{{E1.urgent.probability}}`.
- Needs a decision model role, `decider` by default.

## When to use it

- Put the data in `state` and keep the questions short.
