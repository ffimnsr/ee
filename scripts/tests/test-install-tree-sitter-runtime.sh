#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_path="$repo_root/scripts/install-tree-sitter-runtime.sh"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

fake_build_script="$tmpdir/fake-build-runtime.sh"
runtime_dir="$tmpdir/runtime"
source_root="$tmpdir/sources"
xdg_data_home="$tmpdir/xdg-data"
default_runtime_dir="$xdg_data_home/ee"

cat >"$fake_build_script" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

source_root=""
output_root=""
force=0
skip_load=0

while (($# > 0)); do
  case "$1" in
    --source-root)
      source_root="$2"
      shift 2
      ;;
    --output-root)
      output_root="$2"
      shift 2
      ;;
    --force)
      force=1
      shift
      ;;
    --skip-load)
      skip_load=1
      shift
      ;;
    *)
      printf 'unexpected arg: %s\n' "$1" >&2
      exit 1
      ;;
  esac
done

mkdir -p "$output_root"
printf '%s\n%s\n%s\n%s\n' "$source_root" "$output_root" "$force" "$skip_load" >"$output_root/build-args.txt"
EOF
chmod +x "$fake_build_script"

output="$(
  EE_TREE_SITTER_BUILD_SCRIPT="$fake_build_script" \
    bash "$script_path" --source-root "$source_root" --runtime-dir "$runtime_dir" --force --skip-load
)"

[[ -f "$runtime_dir/build-args.txt" ]]
mapfile -t build_args <"$runtime_dir/build-args.txt"
[[ "${build_args[0]}" == "$source_root" ]]
[[ "${build_args[1]}" == "$runtime_dir" ]]
[[ "${build_args[2]}" == "1" ]]
[[ "${build_args[3]}" == "1" ]]
[[ "$output" == *"tree-sitter runtime installed at $runtime_dir"* ]]
[[ "$output" == *"runtime path matches user runtime lookup"* ]]

command_output="$(
  EE_TREE_SITTER_BUILD_SCRIPT="$fake_build_script" \
    bash "$script_path" --source-root "$source_root" --runtime-dir "$runtime_dir" -- bash -lc 'printf "%s" "$EE_RUNTIME_DIR"'
)"
[[ "$command_output" == "$runtime_dir" ]]

default_output="$(
  XDG_DATA_HOME="$xdg_data_home" \
    EE_TREE_SITTER_BUILD_SCRIPT="$fake_build_script" \
    bash "$script_path" --source-root "$source_root"
)"
[[ -f "$default_runtime_dir/build-args.txt" ]]
mapfile -t default_build_args <"$default_runtime_dir/build-args.txt"
[[ "${default_build_args[1]}" == "$default_runtime_dir" ]]
[[ "$default_output" == *"tree-sitter runtime installed at $default_runtime_dir"* ]]

printf 'install tree-sitter runtime script passed\n'
