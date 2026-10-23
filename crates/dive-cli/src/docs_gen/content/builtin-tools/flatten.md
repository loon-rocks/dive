---
summary: Flattens a list of lists into one list, with no model call.
---

## Behavior

- `[[a, b], [c]]` becomes `[a, b, c]`.
- `depth` is how many levels are spliced; the default is 1.
- Elements that aren't lists are kept as they are.

## When to use it

- After a `map` whose items each return a list, such as one search per query, before deduping, sorting or ranking the results.
