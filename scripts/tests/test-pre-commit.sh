#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
hook_path="$repo_root/.githooks/pre-commit"

capture_stable_clippy_calls() {
    local package_dirs=("$@")

    (
        set -euo pipefail
        source "$hook_path"

        package_scope=()
        scope_args=()
        needs_rust_checks=1
        needs_workspace_checks=0

        local package_dir
        for package_dir in "${package_dirs[@]}"; do
            add_scoped_package "$package_dir"
        done
        build_scope_args

        cargo() {
            printf '%s\n' "$*"
        }

        run_stable_clippy
    )
}

capture_scope_args() {
    local package_dirs=("$@")

    (
        set -euo pipefail
        source "$hook_path"

        package_scope=()
        scope_args=()
        needs_workspace_checks=0

        local package_dir
        for package_dir in "${package_dirs[@]}"; do
            add_scoped_package "$package_dir"
        done
        build_scope_args

        printf '%s\n' "${scope_args[*]}"
    )
}

ee_cli_output="$(capture_stable_clippy_calls ee-cli)"
[[ "$ee_cli_output" == *"+stable clippy -p ee-cli --lib --bins --tests --examples --all-features -- -D warnings"* ]]

mixed_output="$(capture_stable_clippy_calls ee-cli xi-core-lib)"
[[ "$mixed_output" == *"+stable clippy -p ee-cli -p ee-xi-core-lib --lib --bins --tests --examples --all-features -- -D warnings"* ]]

mixed_scope_args="$(capture_scope_args ee-cli xi-core-lib)"
[[ "$mixed_scope_args" == "-p ee-cli -p ee-xi-core-lib" ]]

printf 'pre-commit hook test passed\n'
