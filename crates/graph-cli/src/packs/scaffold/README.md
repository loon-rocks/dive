# @NAME@

A [graph](https://github.com/tylerdavis/graph) pack. Say here what it does and what it needs.

## Install

```bash
graph pack add <this repo's git URL> --path @NAME@
```

The pack lives in the `@NAME@/` directory of its repository, next to any other packs there, and its releases are the repository's `@NAME@/vX.Y.Z` tags.

## Contents

| Item | What it does |
|---|---|
| `@NAME@__tool__word_count` | Counts the words in a file |
| `@NAME@__plan__count_words` | Returns a file's path and word count |
| `@NAME@__agent__word_counter` | Answers questions about how long files are |

## Layout

| Path | Holds |
|---|---|
| `pack.toml` | the manifest: name, description, requirements |
| `tools/`, `plans/`, `agents/` | the pack's items, in graph's tool, plan and agent formats |
| `plans/migrations/`, `tools/migrations/` | one `migrate_v<N>` plan per major release, and the tools only migrations use |
| `tests/migrations/v<N>/` | sample user files written against the previous major, for `graph pack test` |
| `CHANGELOG.md`, `MIGRATING.md` | what each release changed, and how to move across a major |

Pack files refer to their own items as `$pack__<kind>__<name>` in a step's `tool_name` and an agent's `tools`, `subagents` and `handoffs`, and to the installed name as `{{pack.name}}` in templates, so the pack works under any name it's installed as.

## Develop

Run these from the repository root.

| Command | What it does |
|---|---|
| `graph pack check @NAME@` | loads the pack the way an install would and reports every problem |
| `graph pack run tool__word_count --input file=@NAME@/README.md --path @NAME@` | runs any item, private and migration ones included |
| `graph pack test @NAME@` | runs `check`, then each migration over its sample files; without a directory, tests every pack in the repository |
| `graph pack release <major\|minor\|patch> @NAME@` | compares the public interface with the previous `@NAME@/vX.Y.Z` tag and creates the next one; pushing it is up to you |
