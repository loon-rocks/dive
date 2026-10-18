---
summary: Edits existing agents, plans and tools in a chat thread. Proposes a fix from the call that went wrong, has the builders make it when you say go, and tests it against that call.
---

- Starts an edit session, a chat thread opened on the items to edit and, when the edit comes from a run, the call that went wrong as its evidence. It replies first, without waiting for a message, with what went wrong and the change it proposes.
- Revises the proposal on feedback, and changes nothing until you say go.
- Asks before attaching another item to the edit, saying why it belongs there.
- Runs `tool_drafter`, `plan_drafter` and `agent_drafter` as subagents, one per changed item, all at once.
- Tests the edits against the evidence's input when you ask, and never saves: you save from the edit, which writes each changed item back to its own file, a built-in to a project override and a pack's item to the pack's installed copy.
