#!/usr/bin/env bash
# Sync Zed language catalog into ee.
#
# Fetches upstream zed docs (docs/src/languages/*.md), pins every grammar repo
# to an exact git rev (git ls-remote HEAD), merges scripts/zed-catalog/overrides.toml,
# and regenerates:
#   references/zed-language-catalog.json                      (full manifest)
#   crates/xi-core-lib/src/runtime_loader/builtin_zed_generated.rs
#   crates/xi-lsp-lib/src/bundled_zed_generated.rs
#
# Network: needs raw.githubusercontent.com (docs) + github.com (git ls-remote).
# Regeneration is explicit; run with --refresh-remote only on purpose.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CATALOG_DIR="$REPO_ROOT/scripts/zed-catalog"
DOCS_DIR="$CATALOG_DIR/docs"
MANIFEST="$REPO_ROOT/references/zed-language-catalog.json"
GENERATOR="$CATALOG_DIR/generate.py"
ZED_REF="${ZED_REF:-refs/heads/main}"
BASE_URL="https://raw.githubusercontent.com/zed-industries/zed/${ZED_REF}/docs/src/languages"

# Upstream doc inventory (mirrors docs/src/languages directory listing).
DOCS=(
  ansible asciidoc astro bash biome c clojure cpp csharp css dart deno diff docker
  elixir elm emmet erlang fish gdscript gleam glsl go groovy haskell helm html java
  javascript json jsonnet julia kotlin lua luau makefile markdown nim ocaml opentofu
  php powershell prisma proto purescript python r racket rego roc rst ruby rust scala
  scheme sh sml sql svelte swift tailwindcss terraform toml typescript uiua vue xml
  yaml yara yarn zig
)

usage() {
  cat <<'EOF'
Usage: scripts/zed-catalog/sync.sh [--fetch-only]

  --fetch-only   Fetch docs and refresh manifest pins without emitting Rust.
  (default)      Full sync: fetch docs, pin revs, emit manifest + Rust catalogs.
EOF
}

FETCH_ONLY=0
if (($# > 0)); then
  case "$1" in
    --fetch-only) FETCH_ONLY=1 ;;
    *) usage; exit 2 ;;
  esac
fi

mkdir -p "$DOCS_DIR"

failed=()
for doc in "${DOCS[@]}"; do
  if ! curl -fsSL --retry 3 --retry-delay 1 -o "$DOCS_DIR/$doc.md" "$BASE_URL/$doc.md"; then
    failed+=("$doc")
    echo "warning: failed to fetch $doc" >&2
  fi
done
if ((${#failed[@]} > 0)); then
  echo "warning: ${#failed[@]} docs failed: ${failed[*]}" >&2
fi

EMIT_FLAG=("--emit-rust")
if ((FETCH_ONLY)); then
  EMIT_FLAG=()
fi

python3 "$GENERATOR" \
  --docs-dir "$DOCS_DIR" \
  --overrides "$CATALOG_DIR/overrides.toml" \
  --manifest "$MANIFEST" \
  --queries-root "$REPO_ROOT/runtime/queries" \
  ${EMIT_FLAG[@]} \
  --grammar-rust "$REPO_ROOT/crates/xi-core-lib/src/runtime_loader/builtin_zed_generated.rs" \
  --lsp-rust "$REPO_ROOT/crates/xi-lsp-lib/src/bundled_zed_generated.rs"

echo "sync complete: manifest at $MANIFEST"

if command -v rustfmt >/dev/null 2>&1; then
  rustfmt "$REPO_ROOT/crates/xi-core-lib/src/runtime_loader/builtin_zed_generated.rs" "$REPO_ROOT/crates/xi-lsp-lib/src/bundled_zed_generated.rs" --edition 2021 || true
fi