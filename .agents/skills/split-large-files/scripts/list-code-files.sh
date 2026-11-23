#!/usr/bin/env bash
# List `mk lf` entries that are source code, one `<loc> <path>` per line.
# Non-code files are reported on stderr and never split.
set -euo pipefail

if ! command -v mk >/dev/null 2>&1; then
  echo "mk not found on PATH" >&2
  exit 2
fi

mk_lines="$(mk lf)"
if [ -z "$mk_lines" ]; then
  exit 0
fi

printf '%s\n' "$mk_lines" | while IFS= read -r line; do
  [ -n "$line" ] || continue

  loc="$(printf '%s\n' "$line" | sed -E 's/^[[:space:]]*([0-9]+).*/\1/')"
  path="$(printf '%s\n' "$line" | sed -E 's/^[[:space:]]*[0-9]+[[:space:]]+//')"

  if [ -z "$loc" ] || [ -z "$path" ]; then
    continue
  fi

  case "$path" in
    *.rs|*.ts|*.tsx|*.js|*.jsx|*.mjs|*.cjs|*.py|*.go|*.java|*.kt|*.kts|*.c|*.h|*.cc|*.cpp|*.cxx|*.hpp|*.cs|*.rb|*.php|*.swift|*.scala|*.sh|*.bash|*.zsh|*.lua|*.zig|*.ex|*.exs|*.erl|*.hrl|*.ml|*.mli|*.hs|*.clj|*.cljs|*.fs|*.fsx|*.vb|*.sol|*.dart|*.r|*.R)
      printf '%s %s\n' "$loc" "$path"
      ;;
    *)
      printf 'skip non-code: %s (%s LOC)\n' "$path" "$loc" >&2
      ;;
  esac
done
