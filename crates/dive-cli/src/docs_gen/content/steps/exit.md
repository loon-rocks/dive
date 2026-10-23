---
summary: Ends the plan early, as a success or an error, when a condition holds.
fields:
  passed: Always true. The plan only reaches later steps when the exit didn't fire.
  verdict: Whether the condition held. Always false when later steps can read it.
  reason: Why the gate decided as it did, for `infer` and `decide` gates.
---

## How it works

- Gate it with `when`, `infer` or `decide`. See [Conditions and bodies](/plans/reference/conditions-and-bodies). With no gate, it always exits.
- `status: success` ends the plan with exit code 0. `status: error` ends it with exit code 4.
- `message` explains the exit.
- In an output plan, a success exit's `output` returns exactly the `finish.output` keys. Error exits, and exits in solver or silent plans, take no `output`.
- An exit inside a `route`, `map` or `reduce` body ends the whole plan.

## When to use it

- Stop with success when there's nothing to do, such as an empty search.
- Stop with an error to fail a check, such as in CI.
