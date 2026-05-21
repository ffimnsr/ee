#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/install-plugins.sh [options]

Install bundled plugins into user config plugin directory so cargo-installed
or locally-built `ee` can discover them without bundled release layout.

Options:
  --plugin-dir DIR     Directory that will receive installed plugins.
  --manifest-path FILE Plugin manifest source path.
  --binary-path FILE   Prebuilt plugin binary source path. Skips cargo build.
  --debug              Build plugin with debug profile instead of release.
  -h, --help           Show this help.

Examples:
  scripts/install-plugins.sh
  scripts/install-plugins.sh --debug
  scripts/install-plugins.sh --plugin-dir "$HOME/.config/ee/plugins"
EOF
}

main() {
  local repo_root
  repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  local config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
  local plugin_dir="$config_home/ee/plugins"
  local manifest_path="$repo_root/crates/xi-lsp-lib/manifest.toml"
  local binary_path=""
  local profile="release"

  while (($# > 0)); do
    case "$1" in
      --plugin-dir)
        plugin_dir="$2"
        shift 2
        ;;
      --manifest-path)
        manifest_path="$2"
        shift 2
        ;;
      --binary-path)
        binary_path="$2"
        shift 2
        ;;
      --debug)
        profile="debug"
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

  if [[ ! -f "$manifest_path" ]]; then
    printf 'error: plugin manifest not found: %s\n' "$manifest_path" >&2
    exit 1
  fi

  if [[ -z "$binary_path" ]]; then
    local cargo_cmd=(cargo build -p ee-xi-lsp-lib --bin xi-lsp-plugin)
    if [[ "$profile" == "release" ]]; then
      cargo_cmd+=(--release)
    fi
    (
      cd "$repo_root"
      "${cargo_cmd[@]}"
    )
    binary_path="${CARGO_TARGET_DIR:-$repo_root/target}/$profile/xi-lsp-plugin${EXE_SUFFIX:-}"
  fi

  if [[ ! -f "$binary_path" ]]; then
    printf 'error: plugin binary not found: %s\n' "$binary_path" >&2
    exit 1
  fi

  local install_dir="$plugin_dir/xi-lsp-plugin"
  mkdir -p "$install_dir/bin"
  cp "$manifest_path" "$install_dir/manifest.toml"
  cp "$binary_path" "$install_dir/bin/xi-lsp-plugin${EXE_SUFFIX:-}"
  chmod +x "$install_dir/bin/xi-lsp-plugin${EXE_SUFFIX:-}"

  printf 'bundled plugins installed at %s\n' "$plugin_dir"
  printf 'installed plugin: xi-lsp-plugin\n'
}

main "$@"