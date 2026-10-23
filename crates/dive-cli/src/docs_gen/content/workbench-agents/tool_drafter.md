---
summary: Builds one tool node of the architect's sketch as a user tool file and test-runs it.
---

- `network_drafter` and `network_editor` run it as a subagent; it never talks to you.
- In an edit session, either patches one change or reworks one portion of the opened file to `network_editor`'s mini-sketch, keeping everything else byte-identical, name and interface included.
- Applies the same design principles as `network_drafter`, decision models first.
- Test runs need your approval.
- Returns a question instead of guessing a value only you know.
