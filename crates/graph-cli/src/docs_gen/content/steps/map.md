---
summary: Runs the same steps once for each item in a list and collects the results.
fields:
  count: How many items ran.
  results: Each item's result, in input order.
  failed_count: "With `onError: skip`: how many items failed."
  failed: "With `onError: skip`: the items that failed."
  failed.[].index: The failed item's position in the list.
  failed.[].item: The failed item.
  failed.[].error: Why it failed.
---

## How it works

- `over` must produce a list. The body (`do`) runs once per item, with `{{item}}` and `{{index}}` available.
- `concurrency` runs that many items at once. Use it only when items don't depend on each other.
- `onError: skip` leaves failed items out of `results` and lists them in `failed`, with `failed_count`, instead of failing the plan.

## Limits

- The body can contain `agent`, `ask`, `filter`, `route` and `exit`, but not another `map` or `reduce`. Call a plan for nested iteration.
- `ask` steps in the body run one item at a time.
