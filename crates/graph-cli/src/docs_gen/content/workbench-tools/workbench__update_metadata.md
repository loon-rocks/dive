---
summary: Updates the plan draft's top-level fields.
---

## Fields

- `identifier`, `name`, `description`, `exemplars`, `input_schema`, `requires_servers`.
- `finish` sets the result: a `solver`, an `output` map, or neither for a silent plan.

## Notes

- Changing `identifier` makes it a new plan.
