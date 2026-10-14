---
summary: Model calls and JSON reshaping that every agent and plan step can use, with no pack to enable.
---

- Always in the catalog, called as `builtin__<name>`.
- `infer` and `decide` need a configured model: a chat model for `infer`, a decision model for `decide`. `reshape` makes no model call.
- To replace one, put a tool file with the same `name` in `<tools dir>/builtin/`, such as `.graph/tools/builtin/reshape.yaml`.
