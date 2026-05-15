#!/usr/bin/env bash

set -euo pipefail

if ! command -v hyperfine >/dev/null 2>&1; then
  echo "hyperfine is required to run this script. Please install it and try again."
  exit 1
fi

hyperfine --warmup 2 "wc -l ./test_assets/vbig-10gb.txt" "./target/release/ee do file line-check ./test_assets/vbig-10gb.txt"