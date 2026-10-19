---
summary: Sorts a list by a field, with no model call.
---

## Behavior

- `by` is the field, as a dot path such as `score` or `meta.date`. Without `by`, the items themselves are compared.
- `order` is `asc` (the default) or `desc`.
- Numbers compare as numbers and strings as text, so ISO dates sort correctly. The sort is stable.
- Items missing the field go last in either order.

## When to use it

- Ordering by a value the items already carry: a date, a count, a score from an earlier step.
- To order items by how well they fit a question, use [`builtin__rank`](/tools/reference/builtin/rank).
