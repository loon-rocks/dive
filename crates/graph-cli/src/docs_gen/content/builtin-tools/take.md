---
summary: Keeps the first n items of a list, with no model call.
---

## Behavior

- Returns the first `n` items; a shorter list is returned whole.

## When to use it

- The top-k cut after [`builtin__sort`](/tools/reference/builtin/sort). [`builtin__rank`](/tools/reference/builtin/rank) cuts its own result with `top_k`.
