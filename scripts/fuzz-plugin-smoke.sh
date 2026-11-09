#!/bin/sh
# Replay bounded inputs through the v2 plugin fuzz harness.
#
# `cargo fuzz` needs nightly and is excluded from the workspace, so this runs the
# harness directly with crafted and arbitrary bounded inputs. The fuzz package
# lives outside the workspace; the manifest path selects it explicitly.
set -eu

cd "$(dirname "$0")/.."

scripts/run-tests-memguard.sh cargo test --quiet \
    --manifest-path fuzz/Cargo.toml \
    --test ee_plugin_wasm_smoke
