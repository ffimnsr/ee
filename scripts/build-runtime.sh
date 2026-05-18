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
  -h, --help         Show this help.
EOF
}

main() {
  local source_root="target/runtime-sources"
  local output_root="target/runtime-package"
  local force=0
  local skip_load=0

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

  local force_args=()
  if (( force )); then
    force_args+=(--force)
  fi

  local runtime_build_args=()
  if (( skip_load )); then
    runtime_build_args+=(--skip-load)
  fi

  cargo run --locked -p ee-cli -- do runtime-fetch --all --source-root "$source_root" "${force_args[@]}"
  cargo run --locked -p ee-cli -- do runtime-build --all --source-root "$source_root" --output-root "$output_root" "${force_args[@]}" "${runtime_build_args[@]}"
}

main "$@"
