---
summary: Runs one model call over an instruction and returns text or structured JSON.
---

## Behavior

- Returns `{text}` by default.
- With `output_schema`, returns JSON validated against it, so every declared field is present.
- `model` picks a model role; omit it for the chat role.

## When to use it

- Classify, extract or summarize inside a plan.
- Inside a `map`, interpolate `{{item}}` per call instead of passing the whole list.
