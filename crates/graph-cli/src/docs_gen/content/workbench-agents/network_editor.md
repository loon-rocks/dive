---
summary: Edits existing agents, plans and tools in a chat thread. Proposes a fix from the call that went wrong, has the builders make it when you say go, and tests it against that call.
---

- Starts an edit session, a chat thread opened on the items to edit and, when the edit comes from a run, the call that went wrong as its evidence. It replies first, without waiting for a message, with what went wrong and the change it proposes.
- Diagnoses the cause as a design choice, not only the failing line, and picks the smallest fix that removes it: a patch, a restructure of one portion (a map/reduce on a small model, a decide gate, a composed plan), or a rethink of the item's role (an agent becomes a plan, a tool splits).
- Judges the item against the same design principles as `network_drafter`, decision models first. When a restructure beats the patch, shows both side by side with their rough model calls and cost, and you pick.
- Hands a rework to the builder as a mini-sketch of the reworked portion, with each step's tool and model role.
- Asks one question, with its guess, only when it can't settle the diagnosis.
- Revises the proposal on feedback, and changes nothing until you say go.
- Asks before attaching another item to the edit, saying why it belongs there.
- Runs `tool_drafter`, `plan_drafter` and `agent_drafter` as subagents, one per changed item, all at once.
- Tests the edits against the evidence's input when you ask, and never saves: you save from the edit, which writes each changed item back to its own file, a built-in to a project override and a pack's item to the pack's installed copy.
