#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
summary_script="$repo_root/scripts/test-workspace-summary.sh"
log_file="$(mktemp)"
trap 'rm -f "$log_file"' EXIT

cat >"$log_file" <<'EOF'
test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 2 filtered out; finished in 0.01s

running 2 tests
test alpha::works ... FAILED
test beta::fails ... FAILED

failures:
    alpha::works
    beta::fails

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s

error: test failed, to rerun pass `-p ee-cli --lib`
EOF

output="$(bash "$summary_script" --parse-only "$log_file")"
print_command_output="$(bash "$summary_script" --print-command +stable --workspace --all-features)"

[[ "$output" == *"passed: 4"* ]]
[[ "$output" == *"failed: 2"* ]]
[[ "$output" == *"ignored: 1"* ]]
[[ "$output" == *"filtered out: 2"* ]]
[[ "$output" == *"- alpha::works"* ]]
[[ "$output" == *"- beta::fails"* ]]
[[ "$output" == *"- ee-cli"* ]]
[[ "$print_command_output" == "cargo +stable test --workspace --all-features" ]]

printf 'test workspace summary script passed\n'