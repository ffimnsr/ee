#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_path="$repo_root/scripts/gen_text_fixture.sh"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

output_path="$tmpdir/fixture.txt"

bash "$script_path" --seed 7 --line-min 20 --line-max 30 4kb "$output_path" >/dev/null

actual_size="$(wc -c < "$output_path" | tr -d '[:space:]')"
[[ "$actual_size" == "4096" ]]
grep -q ' ' "$output_path"

newline_count="$(awk 'END { print NR }' "$output_path")"
[[ "$newline_count" -ge 10 ]]

invalid_output="$(
    bash "$script_path" 10tb "$tmpdir/invalid.txt" 2>&1 >/dev/null || true
)"
[[ "$invalid_output" == *"invalid size"* ]]

printf 'shell fixture generator test passed\n'
