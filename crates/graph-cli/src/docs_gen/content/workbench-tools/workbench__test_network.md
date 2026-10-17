---
summary: Tests the drafted, unsaved network once, after you approve the run.
---

## Behavior

- Starts at an agent node with a message, or runs a plan or tool node with an input.
- Every agent runs on its own model and tools run for real. Nothing is saved. A test stops after 90 seconds.
- Needs every node drafted without problems, and returns which nodes ran and the answer.
- Before anything runs, collects the secrets every tool the test would call needs, drafted or existing: each `${NAME}` with no default in a tool's `env`. Each resolves from the environment, then the project's stored value, then the global one. When any is missing, nothing runs and the result is `status: missing_secrets`, listing each missing name with the tools that need it and its `description`; the network then waits on those secrets until a test runs.
