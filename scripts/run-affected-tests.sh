#!/bin/sh
# Run focused tests for changed Rust files, then check direct dependents.
# Thin wrapper so the documented entrypoint stays a shell script.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec python3 "$repo_root/scripts/run-affected-tests.py" "$@"
