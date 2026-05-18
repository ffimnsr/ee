#!/usr/bin/env bash

set -euo pipefail

if ! command -v hyperfine >/dev/null 2>&1; then
  echo "hyperfine is required to run this script. Please install it and try again."
  exit 1
fi

hyperfine -N --warmup 2 "head ./test_assets/vbig-10gb.txt" "./target/release/ee do file head ./test_assets/vbig-10gb.txt"