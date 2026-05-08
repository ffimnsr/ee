#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
hook_path="$repo_root/.githooks/pre-commit"

capture_stable_clippy_calls() {
    local package_pairs=("$@")
    local line

    (
        set -euo pipefail
        source "$hook_path"

        package_scope=()
        scope_args=()
        needs_rust_checks=1
        needs_workspace_checks=0

        local pair
        for pair in "${package_pairs[@]}"; do
            package_scope["$pair"]=1
            scope_args+=(-p "$pair")
        done

        cargo() {
            printf '%s\n' "$*"
        }

        run_stable_clippy
    )
}

ee_tui_output="$(capture_stable_clippy_calls ee-tui)"
[[ "$ee_tui_output" == *"+stable clippy -p ee-tui --bins --tests --examples --all-features -- -D warnings"* ]]
[[ "$ee_tui_output" != *"+stable clippy -p ee-tui --lib --bins --tests --examples --all-features -- -D warnings"* ]]

mixed_output="$(capture_stable_clippy_calls ee-tui xi-core-lib)"
[[ "$mixed_output" == *"+stable clippy -p xi-core-lib --lib --bins --tests --examples --all-features -- -D warnings"* ]]
[[ "$mixed_output" == *"+stable clippy -p ee-tui --bins --tests --examples --all-features -- -D warnings"* ]]

printf 'pre-commit hook test passed\n'
