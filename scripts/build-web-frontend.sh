#!/bin/sh
# Builds the Rust/WASM web client into crates/ee-web-frontend/dist.
#
# Requires the wasm32-unknown-unknown target and a matching wasm-bindgen CLI.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"
crate=crates/ee-web-frontend

rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true
cargo build --release -p ee-web-frontend --target wasm32-unknown-unknown

wasm-bindgen \
  --target web \
  --no-typescript \
  --out-name ee_web \
  --out-dir "$crate/dist" \
  target/wasm32-unknown-unknown/release/ee_web_frontend.wasm

cp "$crate/index.html" "$crate/style.css" "$crate/dist/"

size=$(wc -c < "$crate/dist/ee_web_bg.wasm")
cap=2097152
if [ "$size" -gt "$cap" ]; then
  echo "error: ee_web_bg.wasm is $size bytes; cap is $cap" >&2
  exit 1
fi
echo "web frontend dist ready ($size bytes wasm)"
