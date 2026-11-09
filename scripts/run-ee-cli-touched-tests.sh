#!/bin/sh
# Fast feedback path for ee-cli frontend work.
#
# Runs the touched bin-test modules in separate processes so the harness
# releases peak allocations between groups, then runs tests::misc with the
# 20 MB fixtures skipped (run those explicitly when touching file loading).
#
# Usage: scripts/run-ee-cli-touched-tests.sh [module-prefix...]
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

if [ "$#" -gt 0 ]; then
    prefixes=$*
else
    prefixes="tests::notifications tests::lsp_edits tests::commands tests::keybindings tests::keymap_config tests::vim_keys tests::edit_commands tests::render tests::plugin_ui"
fi

for prefix in $prefixes; do
    echo "== $prefix"
    scripts/run-tests-memguard.sh cargo test --quiet -p ee-cli --features agents \
        --bin ee "$prefix" -- --test-threads=1
done

echo "== tests::misc (large-file fixtures skipped)"
scripts/run-tests-memguard.sh cargo test --quiet -p ee-cli --features agents \
    --bin ee tests::misc -- --test-threads=1 \
    --skip open_many_line_20mb \
    --skip open_long_line_20mb
