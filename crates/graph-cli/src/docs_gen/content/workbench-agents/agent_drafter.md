---
summary: Builds one agent node of the architect's sketch as an agent file.
---

- `network_drafter` and `network_editor` run it as a subagent; it never talks to you.
- In an edit session, either patches one change or reworks one portion of the opened file to `network_editor`'s mini-sketch, keeping everything else byte-identical, name and interface included.
- Applies the same design principles as `network_drafter`, decision models first.
- Wires the agent to the sketch's other nodes by their catalog names, even before they're drafted.
