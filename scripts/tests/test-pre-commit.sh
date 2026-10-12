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

(
    set -euo pipefail
    source "$hook_path"
    [[ "${CARGO_INCREMENTAL:-}" == "0" ]]
)

# State-gated test cache: canonical command + record/match roundtrip.
(
    set -euo pipefail
    source "$hook_path"

    state_file="$(mktemp)"
    trap 'rm -f "$state_file"' EXIT
    export EE_PRE_COMMIT_STATE_FILE="$state_file"

    canonical="$(githooks_test_cmd)"
    [[ "$canonical" == "cargo test --quiet --workspace --all-features --locked" ]]
    [[ "$(githooks_state_file)" == "$state_file" ]]

    key="$(githooks_compute_key)"
    [[ -n "$key" ]]

    # No state yet: mismatch, then record, then match.
    ! githooks_state_matches "$canonical" "$key"
    githooks_record_pass "$canonical" "$key"
    githooks_state_matches "$canonical" "$key"

    # A different command or key never matches the recorded pass.
    ! githooks_state_matches "cargo test --quiet --workspace" "$key"
    ! githooks_state_matches "$canonical" "$key"-

    # Key is deterministic for the same tree.
    [[ "$(githooks_compute_key)" == "$key" ]]
)

printf 'pre-commit hook test passed\n'
