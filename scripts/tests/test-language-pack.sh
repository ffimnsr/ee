#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_path="$repo_root/scripts/language-pack.py"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

runtime_root="$tmpdir/runtime"
mkdir -p "$runtime_root/grammars"
printf 'rust grammar\n' >"$runtime_root/grammars/libtree-sitter-rust.so"
printf 'python grammar\n' >"$runtime_root/grammars/libtree-sitter-python.so"
mkdir -p "$runtime_root/grammars/libtree-sitter-rust.dSYM"
printf 'ignored\n' >"$runtime_root/grammars/libtree-sitter-rust.dSYM/Contents"

first="$tmpdir/first.tar.gz"
second="$tmpdir/second.tar.gz"

python3 "$script_path" build \
  --runtime-root "$runtime_root" \
  --output "$first" \
  --version 1.2.3 \
  --target test-target >/dev/null
python3 "$script_path" build \
  --runtime-root "$runtime_root" \
  --output "$second" \
  --version 1.2.3 \
  --target test-target >/dev/null

cmp "$first" "$second"
python3 "$script_path" verify --pack "$first" --version 1.2.3 --target test-target >/dev/null

if python3 "$script_path" verify --pack "$first" --target other-target >/dev/null 2>&1; then
  printf 'expected target mismatch to fail\n' >&2
  exit 1
fi

tar -xOf "$first" manifest.json | python3 -c '
import json, sys
manifest = json.load(sys.stdin)
assert manifest["schema_version"] == 1
assert manifest["package_version"] == "1.2.3"
assert manifest["target"] == "test-target"
paths = [entry["path"] for entry in manifest["files"]]
assert paths == ["grammars/libtree-sitter-python.so", "grammars/libtree-sitter-rust.so"], paths
'

printf 'language pack script passed\n'
