---
summary: Asks a decision model typed questions about one piece of data.
---

## Behavior

- Each question is answered independently, in one call.
- Question types: `likelihood` (a probability), `choice` (one option) and `score` (a level on a scale).
- The result maps each question name to its answer, such as `{{E1.urgent.probability}}`.
- Needs a decision model role, `decider` by default.

## When to use it

- Put the data in `state` and keep the questions short.
