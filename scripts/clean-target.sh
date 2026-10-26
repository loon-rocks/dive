#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
hours="${1:-6}"
target="${CARGO_TARGET_DIR:-$root/target}"

if [[ ! -d "$target" ]]; then
  echo "nothing to clean: $target does not exist"
  exit 0
fi

if stat -c %X / >/dev/null 2>&1; then
  atimes() { stat -c %X "$@"; }
  stamp_date() { date -d "@$1" "$2"; }
else
  atimes() { stat -f %a "$@"; }
  stamp_date() { date -r "$1" "$2"; }
fi

newest="$(find "$target" -path '*/.fingerprint/*' -type f -exec bash -c "$(declare -f atimes); atimes \"\$@\"" _ {} + 2>/dev/null | sort -n | tail -1)"
if [[ -z "$newest" ]]; then
  echo "nothing to clean: no builds in $target"
  exit 0
fi

cutoff=$((newest - hours * 3600))
before="$(du -sh "$target" | cut -f1)"
echo "keeping everything used since $(stamp_date "$cutoff" '+%Y-%m-%d %H:%M'), ${hours}h before the last build at $(stamp_date "$newest" '+%H:%M')"

stamp="$root/sweep.timestamp"
trap 'rm -f "$stamp"' EXIT
printf '{"secs_since_epoch":%d,"nanos_since_epoch":0}' "$cutoff" >"$stamp"
CARGO_TARGET_DIR="$target" cargo sweep --file "$root"

minutes=$((($(date +%s) - cutoff) / 60 + 1))
for dir in "$target"/incremental/* "$target"/*/incremental/* "$target"/*/*/incremental/*; do
  [[ -d "$dir" ]] || continue
  if [[ -z "$(find "$dir" -type f -amin "-$minutes" -print -quit)" ]]; then
    rm -rf "$dir"
  fi
done

echo "$target: $before -> $(du -sh "$target" | cut -f1)"
