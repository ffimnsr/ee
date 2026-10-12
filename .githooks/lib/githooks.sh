#!/usr/bin/env bash
# Shared githooks helpers: test-command canonicalization and the state-gated
# test cache. Sourced by .githooks/pre-commit and .githooks/record-tests-pass;
# functions only, nothing runs on source.
#
# State design: the full workspace test suite is expensive, so it only runs
# when the testable source tree changed since the last recorded pass. The
# state is a content hash over everything a workspace test can depend on
# (tracked plus non-ignored untracked files under `crates/`, the workspace
# manifest, and Cargo.lock), paired with the exact canonical test command.
# README/docs edits do not invalidate the state, and "staged but not yet
# committed" never matters: only file contents do.

# The one source of truth for what "full workspace test" means. The
# pre-commit hook runs exactly this when the state is stale;
# `.githooks/record-tests-pass` records a pass after an outside run, so tests
# run once per tree change no matter who ran them first.
githooks_test_cmd() {
    printf '%s\n' 'cargo test --quiet --workspace --all-features --locked'
}

# State file lives inside target/ (gitignored): `cargo clean` wipes it too,
# forcing a fresh test run.
githooks_state_file() {
    if [ -n "${EE_PRE_COMMIT_STATE_FILE:-}" ]; then
        printf '%s\n' "${EE_PRE_COMMIT_STATE_FILE}"
        return 0
    fi
    printf '%s\n' "$(git rev-parse --show-toplevel)/target/.githooks-state"
}

# Content hash of the test scope: every tracked path under crates/ plus the
# workspace manifest and Cargo.lock, then non-ignored untracked files in the
# same scope (new modules). Output is "<path> <sha256>" lines piped into the
# final digest, so renames and deletions also change the key.
githooks_key_scope() {
    {
        git ls-files -z -- crates Cargo.toml Cargo.lock
        git ls-files -o --exclude-standard -z -- crates Cargo.toml Cargo.lock
    } | sort -z -u | tr '\0' '\n' | while IFS= read -r _f; do
        [ -f "${_f}" ] || continue
        printf '%s ' "${_f}"
        githooks_sha256_file "${_f}"
    done
}

githooks_compute_key() {
    githooks_key_scope | githooks_sha256_stdin
}

githooks_sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi | cut -d' ' -f1
}

githooks_sha256_stdin() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum
    else
        shasum -a 256
    fi | cut -d' ' -f1
}

# Exit 0 when the state file records a pass of exactly $1 with key $2.
githooks_state_matches() {
    local _cmd _key _file
    _cmd="$1"
    _key="$2"
    _file="$(githooks_state_file)"
    [ -f "${_file}" ] || return 1
    [ "$(sed -n 's/^command=//p' "${_file}")" = "${_cmd}" ] || return 1
    [ "$(sed -n 's/^key=//p' "${_file}")" = "${_key}" ] || return 1
}

# Atomically record a pass of $1 (command) with key $2.
githooks_record_pass() {
    local _cmd _key _file _dir _tmp
    _cmd="$1"
    _key="$2"
    _file="$(githooks_state_file)"
    _dir="$(dirname "${_file}")"
    mkdir -p "${_dir}"
    _tmp="${_file}.tmp.$$"
    {
        printf 'command=%s\n' "${_cmd}"
        printf 'key=%s\n' "${_key}"
        printf 'when=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u)"
    } >"${_tmp}"
    mv -f "${_tmp}" "${_file}"
}

# Prints the recorded pass timestamp; exit 1 when there is no state.
githooks_state_when() {
    local _file
    _file="$(githooks_state_file)"
    [ -f "${_file}" ] || return 1
    sed -n 's/^when=//p' "${_file}"
}