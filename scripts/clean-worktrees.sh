#!/usr/bin/env bash
set -euo pipefail

apply=false
if [[ "${1:-}" == "--apply" ]]; then
  apply=true
fi

common_dir=$(git rev-parse --path-format=absolute --git-common-dir)
root=$(dirname "$common_dir")
here=$(git rev-parse --show-toplevel)
git -C "$root" fetch -q origin main

eligible=0
path=""
branch=""

consider() {
  local path=$1 branch=$2 name reason head state
  name=$(basename "$path")
  [[ "$path" == "$root/.claude/worktrees/"* ]] || return 0
  if [[ "$path" == "$here" ]]; then
    echo "keep    $name (current worktree)"
    return 0
  fi
  if [[ -n "$(git -C "$path" status --porcelain 2>/dev/null)" ]]; then
    echo "keep    $name (uncommitted changes)"
    return 0
  fi
  if [[ -z "$branch" ]]; then
    echo "keep    $name (detached HEAD)"
    return 0
  fi
  reason=""
  if [[ "$(git -C "$root" rev-list --count "origin/main..$branch")" == "0" ]]; then
    reason="no commits beyond main"
  else
    head=$(git -C "$root" rev-parse --abbrev-ref "$branch@{upstream}" 2>/dev/null || true)
    head=${head#origin/}
    state=$(gh pr list --head "${head:-$branch}" --state all --json state --jq '.[0].state // ""' 2>/dev/null || true)
    case "$state" in
      MERGED) reason="pull request merged" ;;
      CLOSED) reason="pull request closed" ;;
    esac
  fi
  if [[ -z "$reason" ]]; then
    echo "keep    $name (unmerged work)"
    return 0
  fi
  eligible=$((eligible + 1))
  if $apply; then
    rm -rf "$path/target"
    git -C "$root" worktree remove "$path"
    git -C "$root" branch -D "$branch" >/dev/null
    echo "removed $name ($reason)"
  else
    echo "remove  $name ($reason)"
  fi
}

while IFS= read -r line; do
  case "$line" in
    "worktree "*) path=${line#worktree } ; branch="" ;;
    "branch refs/heads/"*) branch=${line#branch refs/heads/} ;;
    "") [[ -n "$path" ]] && consider "$path" "$branch"; path="" ;;
  esac
done < <(git -C "$root" worktree list --porcelain; echo)

if ! $apply && (( eligible > 0 )); then
  echo
  echo "Run \`mise run worktrees:clean -- --apply\` to remove them."
fi
