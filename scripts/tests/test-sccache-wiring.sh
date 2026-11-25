#!/usr/bin/env bash
# Verifies that local sccache wiring stays opt-in and falls back cleanly.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
wrapper="$repo_root/scripts/sccache-rustc.sh"
bash_path="$(command -v bash)"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

mkdir -p "$tmp_dir/bin" "$tmp_dir/no-sccache"
ln -s "$bash_path" "$tmp_dir/no-sccache/bash"

printf '#!/usr/bin/env bash\nprintf "sccache:%%s\\n" "$*" > "$EE_TEST_LOG"\nexit 0\n' \
    >"$tmp_dir/bin/sccache"
printf '#!/usr/bin/env bash\nprintf "rustc:%%s\\n" "$*" > "$EE_TEST_LOG"\nexit 0\n' \
    >"$tmp_dir/bin/rustc"
chmod +x "$tmp_dir/bin/sccache" "$tmp_dir/bin/rustc"

log_file="$tmp_dir/calls.log"

# sccache wins when installed.
EE_TEST_LOG="$log_file" PATH="$tmp_dir/bin:$PATH" "$wrapper" rustc --version
[[ "$(cat "$log_file")" == "sccache:rustc --version" ]]

# Without sccache the wrapper execs the compiler directly.
rm -f "$log_file"
EE_TEST_LOG="$log_file" PATH="$tmp_dir/no-sccache" "$wrapper" "$tmp_dir/bin/rustc" --version
[[ "$(cat "$log_file")" == "rustc:--version" ]]

# The kill switch bypasses an installed sccache.
rm -f "$log_file"
EE_TEST_LOG="$log_file" EE_DISABLE_SCCACHE=1 PATH="$tmp_dir/bin:$PATH" \
    "$wrapper" rustc --version
[[ "$(cat "$log_file")" == "rustc:--version" ]]

# mk wiring points at the wrapper and the wrapper is executable.
grep -q '^  RUSTC_WRAPPER: ./scripts/sccache-rustc.sh$' "$repo_root/tasks.yaml"
[[ -x "$wrapper" ]]

if command -v mk >/dev/null 2>&1; then
    (cd "$repo_root" && mk validate >/dev/null)
fi

printf 'sccache wiring test passed\n'
