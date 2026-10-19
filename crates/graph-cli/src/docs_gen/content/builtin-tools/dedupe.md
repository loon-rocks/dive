---
summary: Removes duplicates from a list, keeping the first of each, with no model call.
---

## Behavior

- `by` is the field compared, as a dot path such as `url` or `meta.id`. Without `by`, whole items are compared.
- The first item of each key is kept, in the original order.
- Items missing the field are all kept.
- `removed` is how many items were dropped.
