#!/usr/bin/env bash
# Run the CI-equivalent workspace test suite under the memory guard and print a
# one-line summary. Failures still stream to stderr and stop the script.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

started_at="$(date +%s)"
scripts/run-tests-memguard.sh cargo test --quiet --workspace --all-features --locked
finished_at="$(date +%s)"

printf 'workspace test summary: passed in %ss\n' "$((finished_at - started_at))"
