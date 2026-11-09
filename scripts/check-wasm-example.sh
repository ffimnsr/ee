#!/bin/sh
# Build the reference Wasm guest and run its runtime load test.
#
# Requires the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
#
# The runtime test is #[ignore]d in the normal graph because the artifact is a
# cross-target build; this script is the canonical way to exercise it.
set -eu

cd "$(dirname "$0")/.."

cargo build --quiet -p ee-plugin-example-wasm --target wasm32-unknown-unknown --release
scripts/run-tests-memguard.sh cargo test --quiet -p ee-plugin-runtime --features wasm \
    --test wasm_example -- --ignored --nocapture
