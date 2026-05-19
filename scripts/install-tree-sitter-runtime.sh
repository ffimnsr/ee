#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/install-tree-sitter-runtime.sh [options] [-- command...]

Build tree-sitter runtime assets into user bundled-runtime directory for local
testing and optionally run command with EE_RUNTIME_DIR pointed at installed
runtime.

Options:
  --source-root DIR  Directory used to stage grammar source trees.
  --runtime-dir DIR  Directory that will receive installed runtime assets.
  --force            Replace existing staged sources and runtime outputs.
  --skip-load        Skip host-side dynamic library load validation after compile.
  -h, --help         Show this help.

Examples:
  scripts/install-tree-sitter-runtime.sh
  scripts/install-tree-sitter-runtime.sh -- cargo test -p ee-cli
EOF
}

main() {
  local repo_root
  repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  local build_script="${EE_TREE_SITTER_BUILD_SCRIPT:-$repo_root/scripts/build-runtime.sh}"
  local data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
  local source_root="$repo_root/target/test-runtime-sources"
  local runtime_dir="$data_home/ee"
  local force=0
  local skip_load=0
  local command=()

  while (($# > 0)); do
    case "$1" in
      --source-root)
        source_root="$2"
        shift 2
        ;;
      --runtime-dir)
        runtime_dir="$2"
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
      -h|--help)
        usage
        exit 0
        ;;
      --)
        shift
        command=("$@")
        break
        ;;
      *)
        printf 'error: unknown option: %s\n' "$1" >&2
        exit 1
        ;;
    esac
  done

  local build_cmd=(
    bash "$build_script"
    --source-root "$source_root"
    --output-root "$runtime_dir"
  )
  if (( force )); then
    build_cmd+=(--force)
  fi
  if (( skip_load )); then
    build_cmd+=(--skip-load)
  fi

  "${build_cmd[@]}"

  if ((${#command[@]} == 0)); then
    printf 'tree-sitter runtime installed at %s\n' "$runtime_dir"
    printf 'runtime path matches user runtime lookup\n'
    return 0
  fi

  env EE_RUNTIME_DIR="$runtime_dir" "${command[@]}"
}

main "$@"
