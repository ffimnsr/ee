#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_path="$repo_root/scripts/build-runtime.sh"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

fake_ee="$tmpdir/ee"
fetch_args="$tmpdir/fetch-args.txt"
build_args="$tmpdir/build-args.txt"

cat >"$fake_ee" <<EOF
#!/usr/bin/env bash
set -euo pipefail

if [[ "\${1:-}" != "do" || "\${2:-}" != "runtime" ]]; then
  printf 'unexpected command: %s\n' "\$*" >&2
  exit 1
fi

case "\${3:-}" in
  fetch)
    printf '%s\n' "\$*" >"$fetch_args"
    ;;
  build)
    printf '%s\n' "\$*" >"$build_args"
    ;;
  *)
    printf 'unexpected runtime command: %s\n' "\${3:-}" >&2
    exit 1
    ;;
esac
EOF
chmod +x "$fake_ee"

source_root="$tmpdir/sources"
output_root="$tmpdir/output"

bash "$script_path" --ee-bin "$fake_ee" --source-root "$source_root" --output-root "$output_root" >/dev/null

grep -q -- '--language rust' "$fetch_args"
grep -q -- '--language toml' "$fetch_args"
grep -q -- '--language yaml' "$fetch_args"
grep -q -- '--language rust' "$build_args"
grep -q -- '--language toml' "$build_args"
grep -q -- '--language yaml' "$build_args"
! grep -q -- '--all' "$build_args"

bash "$script_path" --ee-bin "$fake_ee" --source-root "$source_root" --output-root "$output_root" \
  --languages python,go --strict >/dev/null

grep -q -- '--language python' "$build_args"
grep -q -- '--language go' "$build_args"
grep -q -- '--strict' "$build_args"
! grep -q -- '--language rust' "$build_args"

bash "$script_path" --ee-bin "$fake_ee" --source-root "$source_root" --output-root "$output_root" \
  --all >/dev/null

grep -q -- '--all' "$build_args"
! grep -q -- '--language' "$build_args"

if bash "$script_path" --ee-bin "$fake_ee" --all --language rust >/dev/null 2>&1; then
  printf 'expected --all with --language to fail\n' >&2
  exit 1
fi

printf 'build runtime script passed\n'
