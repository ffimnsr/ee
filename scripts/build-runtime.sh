#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/build-runtime.sh [options]

Materialize pinned tree-sitter grammar sources and build runtime grammar/query
assets for ee release or local runtime testing.

The default package contains only the languages listed in
`scripts/runtime-default-languages.txt`. Use `--all` for every configured
grammar or `--language` for an explicit set.

Options:
  --all              Build every configured runtime language.
  --languages LIST   Comma- or space-separated language ids (overrides the default list).
  --language LANG    Add one language id; repeat for several languages.
  --source-root DIR  Directory used to stage grammar source trees.
  --output-root DIR  Directory that will receive grammars/ and queries/.
  --force            Replace existing staged sources and grammar outputs.
  --skip-load        Skip host-side dynamic library load validation after compile.
  --strict           Exit non-zero when any selected grammar fails to fetch or build.
  --release          Run the ee helper with the release profile.
  --ee-bin PATH      Use an existing ee binary instead of `cargo run`.
  -h, --help         Show this help.
EOF
}

main() {
  local script_dir
  script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  local default_languages_file="$script_dir/runtime-default-languages.txt"
  local source_root="target/runtime-sources"
  local output_root="target/runtime-package"
  local force=0
  local skip_load=0
  local strict=0
  local release=0
  local ee_bin=""
  local build_all=0
  local -a languages=()
  local -a parsed_languages=()

  while (($# > 0)); do
    case "$1" in
      --all)
        build_all=1
        shift
        ;;
      --languages)
        IFS=', ' read -r -a parsed_languages <<< "$2"
        languages+=("${parsed_languages[@]}")
        shift 2
        ;;
      --language)
        languages+=("$2")
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
      --strict)
        strict=1
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

  if (( build_all )) && ((${#languages[@]} > 0)); then
    printf 'error: --all cannot be combined with --language/--languages\n' >&2
    exit 1
  fi

  if (( ! build_all )) && ((${#languages[@]} == 0)); then
    if [[ ! -f "$default_languages_file" ]]; then
      printf 'error: default language list not found: %s\n' "$default_languages_file" >&2
      exit 1
    fi
    while IFS= read -r language; do
      if [[ -n "$language" ]]; then
        languages+=("$language")
      fi
    done < <(grep -Ev '^[[:space:]]*(#|$)' "$default_languages_file")
  fi

  if (( ! build_all )) && ((${#languages[@]} == 0)); then
    printf 'error: no runtime languages selected\n' >&2
    exit 1
  fi

  local -a selection=()
  if (( build_all )); then
    selection=(--all)
  else
    local language
    for language in "${languages[@]}"; do
      selection+=(--language "$language")
    done
  fi

  local ee_runner=()
  if [[ -n "$ee_bin" ]]; then
    ee_runner=("$ee_bin")
  elif (( release )); then
    ee_runner=(cargo run --release --locked -p ee-cli --)
  else
    ee_runner=(cargo run --locked -p ee-cli --)
  fi

  local runtime_fetch_cmd=(
    "${ee_runner[@]}" do runtime fetch "${selection[@]}" --source-root "$source_root"
  )
  if (( force )); then
    runtime_fetch_cmd+=(--force)
  fi

  local runtime_build_cmd=(
    "${ee_runner[@]}" do runtime build "${selection[@]}" --source-root "$source_root" --output-root "$output_root"
  )
  if (( force )); then
    runtime_build_cmd+=(--force)
  fi
  if (( skip_load )); then
    runtime_build_cmd+=(--skip-load)
  fi
  if (( strict )); then
    runtime_build_cmd+=(--strict)
  fi

  printf 'fetching grammar sources into %s\n' "$source_root" >&2
  "${runtime_fetch_cmd[@]}"

  printf 'fetch complete; building runtime assets into %s\n' "$output_root" >&2
  "${runtime_build_cmd[@]}"
}

main "$@"
