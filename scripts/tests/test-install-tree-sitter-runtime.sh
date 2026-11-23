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
release=0
ee_bin=""
languages=""

while (($# > 0)); do
  case "$1" in
    --all)
      languages+=" --all"
      shift
      ;;
    --languages)
      languages+=" --languages=$2"
      shift 2
      ;;
    --language)
      languages+=" --language=$2"
      shift 2
      ;;
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
    --release)
      release=1
      shift
      ;;
    --ee-bin)
      ee_bin="$2"
      shift 2
      ;;
    *)
      printf 'unexpected arg: %s\n' "$1" >&2
      exit 1
      ;;
  esac
done

mkdir -p "$output_root"
printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
  "$source_root" "$output_root" "$force" "$skip_load" "$release" "$ee_bin" "$languages" >"$output_root/build-args.txt"
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

release_output="$(
  EE_TREE_SITTER_BUILD_SCRIPT="$fake_build_script" \
    bash "$script_path" --source-root "$source_root" --runtime-dir "$runtime_dir" \
      --release --ee-bin /tmp/fake-ee
)"
mapfile -t release_build_args <"$runtime_dir/build-args.txt"
[[ "${release_build_args[4]}" == "1" ]]
[[ "${release_build_args[5]}" == "/tmp/fake-ee" ]]
[[ "$release_output" == *"tree-sitter runtime installed at $runtime_dir"* ]]

language_output="$(
  EE_TREE_SITTER_BUILD_SCRIPT="$fake_build_script" \
    bash "$script_path" --source-root "$source_root" --runtime-dir "$runtime_dir" \
      --language python --language go
)"
mapfile -t language_build_args <"$runtime_dir/build-args.txt"
[[ "${language_build_args[6]}" == " --language=python --language=go" ]]
[[ "$language_output" == *"tree-sitter runtime installed at $runtime_dir"* ]]

all_output="$(
  EE_TREE_SITTER_BUILD_SCRIPT="$fake_build_script" \
    bash "$script_path" --source-root "$source_root" --runtime-dir "$runtime_dir" --all
)"
mapfile -t all_build_args <"$runtime_dir/build-args.txt"
[[ "${all_build_args[6]}" == " --all" ]]
[[ "$all_output" == *"tree-sitter runtime installed at $runtime_dir"* ]]

clean_runtime_dir="$tmpdir/clean-runtime"
mkdir -p "$clean_runtime_dir/grammars"
printf 'stale grammar\n' >"$clean_runtime_dir/grammars/libtree-sitter-old.so"
printf 'kept grammar\n' >"$clean_runtime_dir/grammars/libtree-sitter-kept.so"
printf 'libtree-sitter-kept.so\n' >"$clean_runtime_dir/grammars/.language-pack-installed"

clean_output="$(
  EE_TREE_SITTER_BUILD_SCRIPT="$fake_build_script" \
    bash "$script_path" --source-root "$source_root" --runtime-dir "$clean_runtime_dir" \
      --clean-grammars --language rust
)"
mapfile -t clean_build_args <"$clean_runtime_dir/build-args.txt"
[[ "${clean_build_args[6]}" == " --language=rust" ]]
[[ ! -f "$clean_runtime_dir/grammars/libtree-sitter-old.so" ]]
[[ -f "$clean_runtime_dir/grammars/libtree-sitter-kept.so" ]]
[[ -f "$clean_runtime_dir/grammars/.language-pack-installed" ]]
[[ -z "$(find "$clean_runtime_dir" -maxdepth 1 -name 'grammars.clean.*' -print -quit)" ]]
[[ "$clean_output" == *"tree-sitter runtime installed at $clean_runtime_dir"* ]]

failing_build_script="$tmpdir/failing-build-runtime.sh"
printf '#!/usr/bin/env bash\nexit 1\n' >"$failing_build_script"
chmod +x "$failing_build_script"

restore_runtime_dir="$tmpdir/restore-runtime"
mkdir -p "$restore_runtime_dir/grammars"
printf 'stale grammar\n' >"$restore_runtime_dir/grammars/libtree-sitter-old.so"
printf 'kept grammar\n' >"$restore_runtime_dir/grammars/libtree-sitter-kept.so"
printf 'libtree-sitter-kept.so\n' >"$restore_runtime_dir/grammars/.language-pack-installed"

if EE_TREE_SITTER_BUILD_SCRIPT="$failing_build_script" \
  bash "$script_path" --source-root "$source_root" --runtime-dir "$restore_runtime_dir" \
    --clean-grammars --language rust >/dev/null 2>&1; then
  printf 'expected clean runtime build failure to propagate\n' >&2
  exit 1
fi

[[ -f "$restore_runtime_dir/grammars/libtree-sitter-old.so" ]]
[[ -f "$restore_runtime_dir/grammars/libtree-sitter-kept.so" ]]
[[ -f "$restore_runtime_dir/grammars/.language-pack-installed" ]]
[[ -z "$(find "$restore_runtime_dir" -maxdepth 1 -name 'grammars.clean.*' -print -quit)" ]]

printf 'install tree-sitter runtime script passed\n'
