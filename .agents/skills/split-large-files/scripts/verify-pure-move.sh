#!/usr/bin/env bash
# verify-pure-move.sh - prove a Rust file split is a mechanical move.
#
# Usage:
#   verify-pure-move.sh <baseline.rs> <after-file> [<after-file> ...]
#
# <baseline.rs> is a copy of the original file made before editing.
# <after-file> entries are all files that now hold the moved code: the
# reduced original plus every new module file.
#
# Checks:
#   1. each after-file is < 1000 lines
#   2. `fn` name multiset is identical (count and names)
#   3. call-token multiset is identical (callee identifier before `(`)
#   4. normalized source-line multiset is identical
#
# Normalization ignores only split scaffolding: `use`/`mod` lines, `impl`
# headers, brace-only lines, leading indentation, and `pub` visibility
# prefixes. Everything else (signatures, statements, attributes, comments,
# string contents) must match after leading indentation is ignored.
#
# Import/export scaffolding is deliberately ignored: verify it with cargo check,
# clippy (`unused_imports`, `private_interfaces`), and the skill's public-path
# audit instead.
#
# Exit 0 = pass, 1 = mismatch, 2 = usage/input error.
set -euo pipefail

usage() {
  sed -n '2,22p' "$0" | sed -E 's/^# ?//'
  exit 2
}

if [ "$#" -lt 2 ]; then
  usage
fi

baseline="$1"
shift

if [ ! -f "$baseline" ]; then
  echo "baseline not found: $baseline" >&2
  exit 2
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

union="$tmp/after-union.rs"
: > "$union"

status=0

printf 'baseline: %s (%s LOC)\n' "$baseline" "$(wc -l < "$baseline" | tr -d '[:space:]')"

for f in "$@"; do
  if [ ! -f "$f" ]; then
    echo "after file not found: $f" >&2
    exit 2
  fi

  loc="$(wc -l < "$f" | tr -d '[:space:]')"
  printf 'after:    %s (%s LOC)\n' "$f" "$loc"

  if [ "$loc" -ge 1000 ]; then
    echo "FAIL: $f has $loc lines; must be < 1000" >&2
    status=1
  fi

  printf '\n' >> "$union"
  cat "$f" >> "$union"
done

fn_names() {
  grep -oE '(^|[^[:alnum:]_])(const[[:space:]]+)?(async[[:space:]]+)?(unsafe[[:space:]]+)?(extern[[:space:]]+"[^"]*"[[:space:]]+)?fn[[:space:]]+[[:alpha:]_][[:alnum:]_]*' "$1" 2>/dev/null \
    | sed -E 's/.*fn[[:space:]]+//' \
    | sort || true
}

call_tokens() {
  sed -E 's/pub[[:space:]]*\([[:space:]]*(in[[:space:]]+[^)]*|crate|super|self)[[:space:]]*\)//g; s/pub[[:space:]]+//g' "$1" 2>/dev/null \
    | grep -oE '[[:alpha:]_][[:alnum:]_]*[[:space:]]*\(' \
    | sed -E 's/[[:space:]]*\($//' \
    | grep -vE '^(if|while|for|match|return|fn|let|in|as|move|ref|mut|unsafe|async|extern|where|impl|dyn|self|Self|super|crate|use|mod|struct|enum|trait|type|const|static|loop|else|break|continue|yield|await|box|macro_rules)$' \
    | sort || true
}

norm_lines() {
  awk '
  {
    line = $0
    sub(/\r$/, "", line)
    sub(/^[[:space:]]+/, "", line)
    sub(/[[:space:]]+$/, "", line)
    if (line == "") next

    if (impl_header) {
      if (line ~ /[{]/) impl_header = 0
      next
    }
    if (line ~ /^(unsafe[[:space:]]+)?impl([[:space:]<]|$)/) {
      if (line !~ /[{]/) impl_header = 1
      next
    }

    if (line ~ /^(pub(\([^)]*\))?[[:space:]]+)?use[[:space:]]/) next
    if (line ~ /^(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+[[:alpha:]_][[:alnum:]_]*[[:space:]]*(;|[{])/) next
    if (line == "{" || line == "}" || line == "};") next

    gsub(/pub[[:space:]]*\([[:space:]]*in[[:space:]]+[^)]*\)[[:space:]]*/, "", line)
    gsub(/pub[[:space:]]*\([[:space:]]*(crate|super|self)[[:space:]]*\)[[:space:]]*/, "", line)
    gsub(/pub[[:space:]]+/, "", line)
    sub(/^[[:space:]]+/, "", line)
    if (line == "") next

    print line
  }' "$1" | sort || true
}

fn_names "$baseline" > "$tmp/before.fns"
fn_names "$union" > "$tmp/after.fns"
call_tokens "$baseline" > "$tmp/before.calls"
call_tokens "$union" > "$tmp/after.calls"
norm_lines "$baseline" > "$tmp/before.lines"
norm_lines "$union" > "$tmp/after.lines"

compare() {
  label="$1"
  before="$2"
  after="$3"
  diff_file="$4"

  before_count="$(wc -l < "$before" | tr -d '[:space:]')"
  after_count="$(wc -l < "$after" | tr -d '[:space:]')"

  if diff -u --label "before $label" --label "after $label" "$before" "$after" > "$diff_file"; then
    printf 'PASS %-6s %s -> %s\n' "$label" "$before_count" "$after_count"
  else
    printf 'FAIL %-6s %s -> %s\n' "$label" "$before_count" "$after_count" >&2
    cat "$diff_file" >&2
    status=1
  fi
}

compare "fn" "$tmp/before.fns" "$tmp/after.fns" "$tmp/fn.diff"
compare "calls" "$tmp/before.calls" "$tmp/after.calls" "$tmp/calls.diff"
compare "lines" "$tmp/before.lines" "$tmp/after.lines" "$tmp/lines.diff"

if [ "$status" -ne 0 ]; then
  echo "PURE MOVE NOT PROVEN" >&2
  exit 1
fi

echo "PURE MOVE VERIFIED"
