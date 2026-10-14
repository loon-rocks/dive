---
summary: Validates a plan written as YAML, optionally after applying a patch to it.
---

## Checks

- Structure, templates, and every tool against the catalog.
- Agent steps list their tools; `agent__chat` isn't used.

## Patches

- `patch` replaces steps by id, removes ids, or sets `input_schema` or `output` before checking.
