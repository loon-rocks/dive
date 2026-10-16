---
summary: Tests the drafted, unsaved network once, after you approve the run.
---

## Behavior

- Starts at an agent node with a message, or runs a plan or tool node with an input.
- Every agent runs on its own model and tools run for real. Nothing is saved. A test stops after 90 seconds.
- Needs every node drafted without problems, and returns which nodes ran and the answer.
