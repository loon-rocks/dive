---
summary: Tests the drafted, unsaved network once, after you approve the run.
---

## Behavior

- Starts at an agent node with a message, or runs a plan node with an input.
- Every agent runs on its own model and tools run for real. Nothing is saved.
- Needs every node drafted without problems, and returns which nodes ran and the answer.
