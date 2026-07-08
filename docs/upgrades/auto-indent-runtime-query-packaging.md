# Auto Indent runtime query packaging follow-up

## Finding

`runtime/queries/<language>/indents.scm` existed in repo and runtime loader could compile them at editor load time, but runtime build/install path only copied upstream standard queries fetched from grammar sources:

- `highlights.scm`
- `injections.scm`
- `locals.scm`
- `tags.scm`

That meant `scripts/build-runtime.sh` and `scripts/install-tree-sitter-runtime.sh` could produce installed runtime trees without ee-owned bundled `indents.scm` assets, even though repo runtime carried them.

## Root cause

`RuntimeLoader::build_runtime_assets()` called `copy_standard_queries_to_runtime(...)` only.

File: `crates/xi-core-lib/src/runtime_loader.rs`

Standard-query copy already knew how to materialize grammar-provided assets from fetched sources, but no second step copied ee-owned runtime query files from repo `runtime/queries/` into build output.

## Implementation

Added bundled ee-owned query copy step during runtime build:

1. keep existing upstream standard-query copy
2. add copy of bundled ee-owned query assets from repo `runtime/queries/<language>/`
3. restrict copy to supported ee-owned kinds for that language
4. include copied paths in `RuntimeBuiltGrammar.query_paths`

Current practical effect:

- `scripts/build-runtime.sh` now emits installed `indents.scm` assets for bundled languages
- `scripts/install-tree-sitter-runtime.sh` installs same assets automatically because it already delegates to build script
- no separate installer logic needed

## Validation coverage

Extended runtime build regression coverage so Rust runtime package build must contain `queries/rust/indents.scm`.

This closes packaging gap between:

- repo-bundled runtime query assets
- runtime build output
- user-installed runtime tree
