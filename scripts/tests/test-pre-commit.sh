#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
hook_path="$repo_root/.githooks/pre-commit"

shim_dir="$(mktemp -d)"
trap 'rm -rf "$shim_dir"' EXIT
printf '#!/usr/bin/env bash\nexit 0\n' >"$shim_dir/atlas"
chmod +x "$shim_dir/atlas"

capture_docs_check_call() {
    (
        set -euo pipefail
        source "$hook_path"

        cargo() {
            printf '%s\n' "$*"
        }

        run_docs_check
    )
}

docs_output="$(capture_docs_check_call)"
[[ "$docs_output" == "[pre-commit] docs check"* ]]
[[ "$docs_output" == *"doc --workspace --all-features --no-deps --locked"* ]]

# Full main flow with stubbed cargo and atlas: format, workspace-wide
# clippy, workspace docs, then the state-gated workspace test — runs once,
# records the pass, and is skipped on the second run.
(
    set -euo pipefail
    source "$hook_path"

    cargo() {
        printf 'CALL: %s\n' "$*"
    }

    state_file="$(mktemp)"
    trap 'rm -f "$state_file"' EXIT
    export EE_PRE_COMMIT_STATE_FILE="$state_file"
    export PATH="$shim_dir:$PATH"

    first="$(ATLAS_PRE_COMMIT_FULL=1 main)"
    [[ "$first" == *"CALL: fmt --all --check"* ]]
    [[ "$first" == *"CALL: clippy --workspace --all-targets --all-features -- -D warnings"* ]]
    [[ "$first" != *"+stable"* ]]
    [[ "$first" == *"CALL: doc --workspace --all-features --no-deps --locked"* ]]
    [[ "$first" == *"CALL: test --quiet --workspace --all-features --locked"* ]]
    [[ "$first" == *"tests passed; state recorded in target/.githooks-state"* ]]

    second="$(ATLAS_PRE_COMMIT_FULL=1 main)"
    [[ "$second" == *"tests skipped: workspace test suite already passed for this tree"* ]]
    [[ "$second" != *"CALL: test"* ]]
)

# Drift check: silent when installed hooks are missing/synced, warning when
# the installed copy is out of date.
(
    set -euo pipefail
    source "$hook_path"

    fake_installed="$(mktemp -d)"
    trap 'rm -rf "$fake_installed"' EXIT

    # No installed-hooks dir at all: silent.
    installed_dir="$fake_installed/none"
    export EE_GITHOOKS_INSTALLED_DIR="$installed_dir"
    [[ -z "$(check_hooks_drift)" ]]

    # Empty installed dir: every hook file missing → warnings + hint.
    mkdir -p "$installed_dir"
    drift_out="$(check_hooks_drift)"
    [[ "$drift_out" == *"warning: installed .git/hooks/pre-commit is out of date"* ]]
    [[ "$drift_out" == *"warning: installed .git/hooks/lib/githooks.sh is out of date"* ]]
    [[ "$drift_out" == *"hint: reinstall hooks with: mk install-githooks"* ]]

    # Synced copy: silent.
    cp "$repo_root/.githooks/pre-commit" "$installed_dir/pre-commit"
    mkdir -p "$installed_dir/lib"
    cp "$repo_root/.githooks/lib/githooks.sh" "$installed_dir/lib/githooks.sh"
    cp "$repo_root/.githooks/record-tests-pass" "$installed_dir/record-tests-pass"
    [[ -z "$(check_hooks_drift)" ]]

    # One stale file after sync: warning again.
    printf 'stale\n' >"$installed_dir/pre-commit"
    drift_out="$(check_hooks_drift)"
    [[ "$drift_out" == *"warning: installed .git/hooks/pre-commit is out of date"* ]]
    [[ "$drift_out" != *"lib/githooks.sh is out of date"* ]]
)

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