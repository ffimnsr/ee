#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_path="$repo_root/scripts/install-plugins.sh"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

xdg_config_home="$tmpdir/xdg-config"
plugin_root="$xdg_config_home/ee/plugins"
manifest_path="$tmpdir/manifest.toml"
binary_path="$tmpdir/xi-lsp-plugin"

cat >"$manifest_path" <<'EOF'
manifest_version = 1
name = "xi-lsp-plugin"
version = "0.0.0"
exec_path = "./bin/xi-lsp-plugin"
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

printf 'install plugins script passed\n'