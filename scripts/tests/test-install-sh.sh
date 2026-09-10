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
cat >"$package_root/ee-openrouter-agent" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$package_root/ee-openrouter-agent"

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
bin_dir_without_agent="$tmpdir/bin-without-agent"
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
[[ -f "$bin_dir/ee-openrouter-agent" ]]
[[ -f "$runtime_dir/queries/rust/indents.scm" ]]
[[ -f "$plugin_dir/xi-lsp-plugin/manifest.toml" ]]
[[ -f "$plugin_dir/xi-lsp-plugin/bin/xi-lsp-plugin" ]]
[[ -f "$doc_dir/ee/README.md" ]]
[[ -f "$license_dir/ee/LICENSE" ]]
[[ -f "$license_dir/ee/LICENSE-APACHE" ]]
[[ "$output" == *"Installed ee-openrouter-agent to $bin_dir"* ]]
[[ "$output" == *"Installed tree-sitter runtime to $runtime_dir"* ]]
[[ "$output" == *"Installed bundled plugins to $plugin_dir"* ]]

EE_INSTALL_LOCAL_PACKAGE="$tarball" \
  sh "$script_path" \
    --arch x86_64-unknown-linux-musl \
    --bin-dir "$bin_dir_without_agent" \
    --runtime-dir "$runtime_dir" \
    --plugin-dir "$plugin_dir" \
    --doc-dir "$doc_dir" \
    --license-dir "$license_dir" \
    --without-openrouter-agent \
    --sudo true >/dev/null
[[ ! -e "$bin_dir_without_agent/ee-openrouter-agent" ]]

# ── completions wiring ──────────────────────────────────────────────────────
# Completion generation lives under `ee do completions`; an `ee completions`
# invocation parses as file arguments and launches the editor instead of
# printing a completion script.
! grep -qF '"${_ee}" completions' "$script_path"
grep -qF '"${_ee}" do completions fish' "$script_path"
grep -qF "Run 'ee do completions <shell>' manually." "$script_path"

# Exercise the shipped rc-file helper in isolation: the legacy broken line must
# be replaced, and repeated runs must not duplicate the eval line.
installer_fn="$(sed -n '/^install_rc_completions() {/,/^}$/p' "$script_path")"
[[ -n "$installer_fn" ]] || { echo 'failed to extract install_rc_completions'; exit 1; }
need_cmd() { :; }
eval "$installer_fn"

rc_legacy="$tmpdir/bashrc-legacy"
cat >"$rc_legacy" <<'EOF'
export PATH="$HOME/.local/bin:$PATH"
eval "$(ee completions bash)"
EOF

repair_output="$(install_rc_completions "$rc_legacy" bash)"
[[ "$repair_output" == *"Replaced broken completions line in $rc_legacy"* ]]
[[ "$repair_output" == *"Added completions eval to $rc_legacy"* ]]
! grep -qF 'ee completions bash' "$rc_legacy"
grep -qxF 'eval "$(ee do completions bash)"' "$rc_legacy"
grep -qxF 'export PATH="$HOME/.local/bin:$PATH"' "$rc_legacy"

rc_fresh="$tmpdir/zshrc-fresh"
printf 'alias ll="ls -la"\n' >"$rc_fresh"
install_rc_completions "$rc_fresh" zsh >/dev/null
grep -qxF 'eval "$(ee do completions zsh)"' "$rc_fresh"

install_rc_completions "$rc_fresh" zsh >/dev/null
install_rc_completions "$rc_legacy" bash >/dev/null
[[ "$(grep -cF 'ee do completions' "$rc_fresh" || true)" -eq 1 ]]
[[ "$(grep -cF 'ee do completions' "$rc_legacy" || true)" -eq 1 ]]

printf 'install.sh script passed\n'
