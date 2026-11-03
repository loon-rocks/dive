# Changelog

## v1.0.1

- Graph is now Dive, and this pack's home is now `github.com/loon-rocks/packs`. The pack requires Dive v0.17 or later and installs with `dive pack add github`. The tools are unchanged; the marker examples in their input descriptions now read `dive:pr_review`.

## v1.0.0

- The GitHub and git tools that shipped inside graph up to v0.15 as `builtin__gh_*` and `builtin__git_*`, unchanged, now installed with `graph pack add github` and called as `github__tool__<name>`.
