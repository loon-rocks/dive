---
summary: Builds a value in a new JSON shape from earlier step results.
---

## Behavior

- `shape` is a JSON object whose leaf strings are templates.
- A leaf that is exactly one template keeps the value's type; mixed text becomes a string.
- It only moves data. It can't compute values like sums or conditionals.

## When to use it

- Rename, pick or nest fields between steps.
- To reshape every item in a list, call it inside a `map` and reference `{{item}}`.
