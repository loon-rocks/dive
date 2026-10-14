---
summary: Runs one of two or more branches, depending on a condition.
fields:
  branch: Which branch ran, `then` or `else`, or the case name with `cases`.
  verdict: Whether the condition held.
  reason: Why the gate decided as it did, for `infer` and `decide` gates.
  result: The output of the branch that ran.
---

## How it works

- Gate it with exactly one of `if`, `infer` or `decide`. See [Conditions and bodies](/plans/reference/conditions-and-bodies).
- `then` runs when the condition holds; `else` runs otherwise. Omit `else` to continue without running anything.
- With a decision model, `decide.options` and `cases` turn it into an N-way choice.

## Limits

- A branch is one tool call or a list of steps. It can contain `exit`, `agent`, `ask`, `filter` and `route`, but not `map` or `reduce`.
