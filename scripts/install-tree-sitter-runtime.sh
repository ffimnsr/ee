#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/install-tree-sitter-runtime.sh [options] [-- command...]

Build tree-sitter runtime assets into user bundled-runtime directory for local
testing and optionally run command with EE_RUNTIME_DIR pointed at installed
runtime. The default install contains only
`scripts/runtime-default-languages.txt`; pass `--all` or `--language` for more.

Options:
  --all              Build every configured runtime language.
  --languages LIST   Comma- or space-separated language ids.
  --language LANG    Add one language id; repeat for several languages.
  --source-root DIR  Directory used to stage grammar source trees.
  --runtime-dir DIR  Directory that will receive installed runtime assets.
  --force            Replace existing staged sources and runtime outputs.
  --skip-load        Skip host-side dynamic library load validation after compile.
  --release          Run the ee helper with the release profile.
  --ee-bin PATH      Use an existing ee binary instead of `cargo run`.
  -h, --help         Show this help.

Examples:
  scripts/install-tree-sitter-runtime.sh
  scripts/install-tree-sitter-runtime.sh --all
  scripts/install-tree-sitter-runtime.sh --language python -- cargo test -p ee-cli
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
  local release=0
  local ee_bin=""
  local -a language_args=()
  local command=()

  while (($# > 0)); do
    case "$1" in
      --all)
        language_args+=(--all)
        shift
        ;;
      --languages)
        language_args+=(--languages "$2")
        shift 2
        ;;
      --language)
        language_args+=(--language "$2")
        shift 2
        ;;
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
      --release)
        release=1
        shift
        ;;
      --ee-bin)
        ee_bin="$2"
        shift 2
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
  )
  if ((${#language_args[@]} > 0)); then
    build_cmd+=("${language_args[@]}")
  fi
  build_cmd+=(
    --source-root "$source_root"
    --output-root "$runtime_dir"
  )
  if (( force )); then
    build_cmd+=(--force)
  fi
  if (( skip_load )); then
    build_cmd+=(--skip-load)
  fi
  if (( release )); then
    build_cmd+=(--release)
  fi
  if [[ -n "$ee_bin" ]]; then
    build_cmd+=(--ee-bin "$ee_bin")
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
