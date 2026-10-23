---
summary: Folds a list into one value by running the same steps for each item in order.
fields:
  count: How many items ran.
  result: The final value of the accumulator.
---

## How it works

- `over` must produce a list. The body (`do`) runs once per item, in order.
- `{{accumulator}}` holds the running value, starting at `initial`. Each run's result becomes the next `{{accumulator}}`.
- `{{item}}` and `{{index}}` are available too.

## Limits

- Always runs one item at a time. For independent per-item work, `map` first and reduce its results.
- The body can't contain another `map` or `reduce`.
