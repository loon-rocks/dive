---
summary: Packs are opt-in bundles of tools, called as `builtin__<name>`.
---

## Loading packs

- Enable packs with `packs = ["github", "slack"]` under `[tools]` in config.toml.
- `infer`, `decide` and `reshape` need no pack. See [Built-in tools](/tools/reference/builtin-tools).

## Overriding a pack tool

- Put a tool file with the same `name` in `<tools dir>/builtin/`, such as `.graph/tools/builtin/git_log.yaml`.
- It replaces the pack tool everywhere it's called.
