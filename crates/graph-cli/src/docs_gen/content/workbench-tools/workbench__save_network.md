---
summary: Saves every drafted node of the network after you confirm the list of files.
---

## Behavior

- Writes tools first, then plans, then agents, to this project's `.graph`, to the global config for every project (`scope`), or to the configured directories.
- Leaves out the nodes named in `skip`.
- Needs every node drafted without problems, and a passing test since the network last changed unless `untested` is true. A save without a passing test asks to save the network untested and reports the files as `(untested)`.
