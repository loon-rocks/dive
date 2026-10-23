---
summary: Asks the person running the plan a question and uses their answer.
fields:
  answer: The answer, matching `outputSchema`.
  answered: False when the person declined or nobody could be asked.
  reason: "When `answered` is false: `declined` or `unavailable`."
---

## When to use it

- For a value only the person running the plan knows, or a confirmation before an irreversible action.
- Not for anything a tool can look up or a gate can judge.

## How it works

- `outputSchema` must be a flat object of strings, numbers, booleans and enums.
- The terminal, the workbench and MCP clients each show the question their own way.
- `whenUnanswered` decides what happens when nobody can answer, such as in CI: `fail` (the default) or `default`, which uses `default`.
