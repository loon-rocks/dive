# github

A [graph](https://github.com/tylerdavis/graph) pack for reading and commenting on pull requests, releases and git history. Its tools run `git`, `gh` and `jq`, so plans can review a pull request, write release notes or summarize what changed between two refs.

## Install

```bash
graph pack add github
```

That installs the pack into the project's `.graph/packs/github`. Add `--global` to install it for every project.

## Requirements

- macOS or Linux.
- `bash`, `git`, `gh` and `jq` on your `PATH`.
- `GH_TOKEN`, or `gh auth login`, for the `gh_*` tools.
- A checkout whose `origin` points at the repository.

## Tools

| Tool | What it does |
|---|---|
| `github__tool__git_log` | Lists the commits between two git refs, newest first |
| `github__tool__git_diff` | Returns the unified diff between two git refs |
| `github__tool__git_changed_files` | Lists the files changed between two git refs, with per-file status and line counts |
| `github__tool__git_file` | Returns a file's contents at a git ref |
| `github__tool__git_grep` | Searches tracked files at a git ref with a regular expression |
| `github__tool__gh_pr_meta` | Returns a pull request's title, body, state, author and base and head commits |
| `github__tool__gh_pr_ticket` | Finds a ticket identifier, such as `LN-1234`, in a pull request |
| `github__tool__gh_pr_comment` | Posts or updates a comment on a pull request |
| `github__tool__gh_pr_inline_comments` | Posts inline review comments on a pull request's diff |
| `github__tool__gh_pr_review_threads` | Lists the review threads a plan posted on a pull request, as structured findings |
| `github__tool__gh_pr_thread_sync` | Syncs a plan's inline review threads on a pull request |
| `github__tool__gh_release` | Returns a release's tag, title, notes and date, plus the previous release's tag |

`gh_pr_comment`, `gh_pr_inline_comments` and `gh_pr_thread_sync` post to GitHub; every other tool is read-only.

## Upgrading from graph v0.15

These tools shipped inside graph up to v0.15 as `builtin__gh_*` and `builtin__git_*`, enabled with `[tools].packs = ["github"]`. From graph v0.16, install this pack with `graph pack add github` and call `github__tool__<name>` wherever your plans and agents called `builtin__<name>`.

## Develop

| Command | What it does |
|---|---|
| `graph pack check` | loads the pack the way an install would and reports every problem |
| `graph pack run tool__git_log --input base=HEAD~5 --input head=HEAD` | runs one of the pack's tools |
| `graph pack test` | runs `check`, then each migration over its sample files |
| `graph pack release <major\|minor\|patch>` | compares the public interface with the previous tag and creates the next tag |
