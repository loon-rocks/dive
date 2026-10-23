---
summary: Saves a drafted tool to the project's tools directory and loads it immediately as `user__<name>`.
---

- The tool must pass `builtin__try_tool` first, unless `untested` is true.
- Won't replace an existing file unless `overwrite` is true.
