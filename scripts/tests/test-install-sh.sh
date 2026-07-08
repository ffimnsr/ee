#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_path="$repo_root/install.sh"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

package_root="$tmpdir/ee-x86_64-unknown-linux-musl"
mkdir -p "$package_root/share/ee/queries/rust"
mkdir -p "$package_root/share/ee/plugins/xi-lsp-plugin/bin"
mkdir -p "$package_root/share/ee/grammars"

cat >"$package_root/ee" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$package_root/ee"

cat >"$package_root/README.md" <<'EOF'
# ee
EOF
cat >"$package_root/LICENSE" <<'EOF'
license
EOF
cat >"$package_root/LICENSE-APACHE" <<'EOF'
apache
EOF
cat >"$package_root/share/ee/queries/rust/indents.scm" <<'EOF'
((block) @indent)
EOF
cat >"$package_root/share/ee/plugins/xi-lsp-plugin/manifest.toml" <<'EOF'
manifest_version = 1
name = "xi-lsp-plugin"
version = "0.0.0"
exec_path = "./bin/xi-lsp-plugin"
EOF
cat >"$package_root/share/ee/plugins/xi-lsp-plugin/bin/xi-lsp-plugin" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$package_root/share/ee/plugins/xi-lsp-plugin/bin/xi-lsp-plugin"

tarball="$tmpdir/ee-x86_64-unknown-linux-musl.tar.gz"
(
  cd "$tmpdir"
  tar -czf "$tarball" "$(basename "$package_root")"
)

bin_dir="$tmpdir/bin"
runtime_dir="$tmpdir/runtime"
plugin_dir="$tmpdir/plugins"
doc_dir="$tmpdir/doc"
license_dir="$tmpdir/licenses"

output="$({
  EE_INSTALL_LOCAL_PACKAGE="$tarball" \
    sh "$script_path" \
      --arch x86_64-unknown-linux-musl \
      --bin-dir "$bin_dir" \
      --runtime-dir "$runtime_dir" \
      --plugin-dir "$plugin_dir" \
      --doc-dir "$doc_dir" \
      --license-dir "$license_dir" \
      --sudo true
})"

[[ -f "$bin_dir/ee" ]]
[[ -f "$runtime_dir/queries/rust/indents.scm" ]]
[[ -f "$plugin_dir/xi-lsp-plugin/manifest.toml" ]]
[[ -f "$plugin_dir/xi-lsp-plugin/bin/xi-lsp-plugin" ]]
[[ -f "$doc_dir/ee/README.md" ]]
[[ -f "$license_dir/ee/LICENSE" ]]
[[ -f "$license_dir/ee/LICENSE-APACHE" ]]
[[ "$output" == *"Installed tree-sitter runtime to $runtime_dir"* ]]
[[ "$output" == *"Installed bundled plugins to $plugin_dir"* ]]

printf 'install.sh script passed\n'
