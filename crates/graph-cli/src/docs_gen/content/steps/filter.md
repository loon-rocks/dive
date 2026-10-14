---
summary: Splits a list into the items that pass a per-item condition and the items that don't.
fields:
  count: How many items passed.
  items: The items that passed, in their original order.
  dropped: The items that didn't pass.
  dropped_count: How many items didn't pass.
  probabilities: "With `decide`: each item's probability, in list order."
---

## How it works

- `over` must produce a list. The gate runs once per item, with `{{item}}` and `{{index}}` available.
- Gate it with exactly one of `where`, `infer` or `decide`. See [Conditions and bodies](/plans/reference/conditions-and-bodies).
- With `decide`, the result also includes each item's `probabilities`.

## When to use it

- Narrow a list before `map`, so later steps only see items they can handle.
- Prefer `where` when a field comparison decides. `infer` and `decide` make one model call per item; set `concurrency` to run them in parallel.
