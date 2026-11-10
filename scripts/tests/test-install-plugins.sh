#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_path="$repo_root/scripts/install-plugins.sh"
tmpdir="$(mktemp -d)"
running_plugin_pid=""
cleanup() {
  if [[ -n "$running_plugin_pid" ]]; then
    kill "$running_plugin_pid" 2>/dev/null || true
    wait "$running_plugin_pid" 2>/dev/null || true
  fi
  rm -rf "$tmpdir"
}
trap cleanup EXIT

xdg_config_home="$tmpdir/xdg-config"
plugin_root="$xdg_config_home/ee/plugins"
manifest_path="$tmpdir/manifest.toml"
binary_path="$tmpdir/xi-lsp-plugin"

cat >"$manifest_path" <<'EOF'
manifest_version = 2
name = "xi-lsp-plugin"
version = "0.0.0"
api = "ee-plugin/2"
targets = ["editor"]
kind = "native"
entry = "bin/xi-lsp-plugin"
EOF

printf '#!/usr/bin/env bash\nexit 0\n' >"$binary_path"
chmod +x "$binary_path"

output="$({
  XDG_CONFIG_HOME="$xdg_config_home" \
    bash "$script_path" \
      --manifest-path "$manifest_path" \
      --binary-path "$binary_path"
})"

installed_plugin_dir="$plugin_root/xi-lsp-plugin"
installed_binary="$installed_plugin_dir/bin/xi-lsp-plugin"

[[ -f "$installed_plugin_dir/manifest.toml" ]]
[[ -f "$installed_binary" ]]
cmp "$manifest_path" "$installed_plugin_dir/manifest.toml"
cmp "$binary_path" "$installed_binary"
[[ "$output" == *"bundled plugins installed at $plugin_root"* ]]
[[ "$output" == *"installed plugin: xi-lsp-plugin"* ]]

running_binary_source="$tmpdir/running-plugin"
replacement_binary_source="$tmpdir/replacement-plugin"
cp "$(type -P sleep)" "$running_binary_source"
cp "$(type -P true)" "$replacement_binary_source"
bash "$script_path" \
  --plugin-dir "$plugin_root" \
  --manifest-path "$manifest_path" \
  --binary-path "$running_binary_source" \
  >/dev/null
"$installed_binary" 30 &
running_plugin_pid=$!
kill -0 "$running_plugin_pid"
bash "$script_path" \
  --plugin-dir "$plugin_root" \
  --manifest-path "$manifest_path" \
  --binary-path "$replacement_binary_source" \
  >/dev/null
cmp "$replacement_binary_source" "$installed_binary"
kill "$running_plugin_pid"
wait "$running_plugin_pid" 2>/dev/null || true
running_plugin_pid=""

custom_plugin_root="$tmpdir/custom-plugins"
custom_output="$({
  bash "$script_path" \
    --plugin-dir "$custom_plugin_root" \
    --manifest-path "$manifest_path" \
    --binary-path "$binary_path"
})"

[[ -f "$custom_plugin_root/xi-lsp-plugin/manifest.toml" ]]
[[ -f "$custom_plugin_root/xi-lsp-plugin/bin/xi-lsp-plugin" ]]
[[ "$custom_output" == *"bundled plugins installed at $custom_plugin_root"* ]]

# A reinstall without --force overwrites known files but keeps stale ones.
stale_marker="$installed_plugin_dir/stale-v1-artifact"
sibling_dir="$plugin_root/other-plugin"
printf 'legacy\n' >"$stale_marker"
mkdir -p "$sibling_dir"
printf 'keep\n' >"$sibling_dir/keep.txt"

bash "$script_path" \
  --plugin-dir "$plugin_root" \
  --manifest-path "$manifest_path" \
  --binary-path "$binary_path" \
  >/dev/null
[[ -f "$stale_marker" ]]

# --force drops the plugin's previous directory so no stale artifact survives.
force_output="$({
  bash "$script_path" \
    --plugin-dir "$plugin_root" \
    --manifest-path "$manifest_path" \
    --binary-path "$binary_path" \
    --force
})" 
[[ ! -e "$stale_marker" ]]
[[ -f "$installed_plugin_dir/manifest.toml" ]]
[[ -f "$installed_binary" ]]
cmp "$binary_path" "$installed_binary"
[[ "$force_output" == *"removed previous installation at $installed_plugin_dir"* ]]
[[ -f "$sibling_dir/keep.txt" ]]

unsafe_manifest="$tmpdir/unsafe-manifest.toml"
cat >"$unsafe_manifest" <<'EOF'
manifest_version = 2
name = "../escape"
kind = "native"
entry = "bin/xi-lsp-plugin"
EOF
if bash "$script_path" \
  --plugin-dir "$tmpdir/unsafe-plugins" \
  --manifest-path "$unsafe_manifest" \
  --binary-path "$binary_path" \
  --force 2>"$tmpdir/unsafe-output"; then
  echo 'install-plugins.sh accepted an unsafe plugin name' >&2
  exit 1
fi
grep -qF 'invalid plugin name' "$tmpdir/unsafe-output"
[[ ! -e "$tmpdir/escape" ]]

invalid_manifest="$tmpdir/invalid-manifest.toml"
legacy_manifest_version=1
cat >"$invalid_manifest" <<EOF
manifest_version = ${legacy_manifest_version}
name = "xi-lsp-plugin"
version = "0.0.0"
exec_path = "./bin/xi-lsp-plugin"
EOF
if bash "$script_path" \
  --plugin-dir "$tmpdir/invalid-plugins" \
  --manifest-path "$invalid_manifest" \
  --binary-path "$binary_path" 2>"$tmpdir/invalid-output"; then
  echo 'install-plugins.sh accepted a v1 manifest' >&2
  exit 1
fi
grep -qF 'expected manifest_version = 2' "$tmpdir/invalid-output"

printf 'install plugins script passed\n'