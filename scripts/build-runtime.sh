#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/build-runtime.sh [options]

Materialize pinned tree-sitter grammar sources from the cargo registry and build
runtime grammar/query assets for ee release or local runtime testing.

Options:
  --source-root DIR  Directory used to stage grammar source trees.
  --output-root DIR  Directory that will receive grammars/ and queries/.
  --force            Replace existing staged sources and grammar outputs.
  --skip-load        Skip host-side dynamic library load validation after compile.
  --release          Run the ee helper with the release profile.
  --ee-bin PATH      Use an existing ee binary instead of `cargo run`.
  -h, --help         Show this help.
EOF
}

main() {
  local source_root="target/runtime-sources"
  local output_root="target/runtime-package"
  local force=0
  local skip_load=0
  local release=0
  local ee_bin=""

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
      *)
        printf 'error: unknown option: %s\n' "$1" >&2
        exit 1
        ;;
    esac
  done

  local ee_runner=()
  if [[ -n "$ee_bin" ]]; then
    ee_runner=("$ee_bin")
  elif (( release )); then
    ee_runner=(cargo run --release --locked -p ee-cli --)
  else
    ee_runner=(cargo run --locked -p ee-cli --)
  fi

  local runtime_fetch_cmd=(
    "${ee_runner[@]}" do runtime fetch --all --source-root "$source_root"
  )
  if (( force )); then
    runtime_fetch_cmd+=(--force)
  fi

  local runtime_build_cmd=(
    "${ee_runner[@]}" do runtime build --all --source-root "$source_root" --output-root "$output_root"
  )
  if (( force )); then
    runtime_build_cmd+=(--force)
  fi
  if (( skip_load )); then
    runtime_build_cmd+=(--skip-load)
  fi

  "${runtime_fetch_cmd[@]}"
  "${runtime_build_cmd[@]}"
}

main "$@"
