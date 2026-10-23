---
summary: Model calls, list processing and JSON reshaping that every agent and plan step can use, with no pack to enable.
---

- Always in the catalog, called as `builtin__<name>`.
- `infer` and `decide` need a configured model: a chat model for `infer`, a decision model for `decide`. `rank` needs a decision model too, and is listed only when one is configured.
- `reshape`, `flatten`, `dedupe`, `sort` and `take` make no model call.
- To replace `infer`, `decide` or `reshape`, put a tool file with the same `name` in `<tools dir>/builtin/`, such as `.dive/tools/builtin/reshape.yaml`. The list tools can't be replaced.
