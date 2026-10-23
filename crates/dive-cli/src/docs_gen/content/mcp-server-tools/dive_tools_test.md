---
summary: Calls one tool directly and returns its result.
---

## Behavior

- Use it to see a tool's real output before writing templates against it.
- A tool that reports failure returns `isError: true` instead of failing the call.
- Control steps can't be tested this way; run a plan instead.
- Mirrors `dive tool test`.
