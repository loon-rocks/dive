---
summary: Opens an existing plan, agent or tool as a node of the network draft, or a plan in the plan pane when there's no sketch and no items being edited.
---

## Behavior

- Opens user tools, built-in tools (`builtin__<name>`) and a pack's tools (`<pack>__tool__<name>`), and agents and plans wherever they come from.
- Saving the network later writes the item back: to its own file, to a project override for a built-in, or to the pack's installed copy for a pack's item, which `dive pack update` replaces.
- In a thread that is already editing items, needs a `reason` and asks you to approve attaching the item before it opens.
- Loading a plan into the plan pane over a draft with unsaved changes fails unless `overwrite_draft` is true.
