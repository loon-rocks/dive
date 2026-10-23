---
summary: Saves every changed node of the network after you confirm the list of files.
---

## Behavior

- Writes tools first, then plans, then agents, to this project's `.dive`, to the global config for every project (`scope`), or to the configured directories.
- Writes an opened item back to its own file, a built-in to a project override (`tools/builtin/<name>.yaml` for a tool, an agent or plan file of the same name otherwise), and a pack's item to the pack's installed copy. The result lists each file with where it went: `file`, `override`, `pack` or `new`.
- Leaves out the nodes named in `skip`, and opened items that haven't changed.
- Needs every node drafted without problems, and a passing test since the network last changed unless `untested` is true. A save without a passing test asks to save the network untested and reports the files as `(untested)`.
- In an edit without a sketch, the saved items stay open, so the next change starts from what was saved.
