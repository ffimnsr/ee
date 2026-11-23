---
id: bf19bdecfa707e672d538ea9d5bb3bcd
label: mk-install-default-runtime-prune
source_type: handoff
session_id: 96ad84220b620129f2575182a253e5490d6991f0d249559734d4bca6913ec159
agent_id: null
content_type: text/markdown
identity_kind: artifact_label
identity_value: mk-install-default-runtime-prune
repo_roots:
  - /home/pastel/Projects/ee
repo_ids: []
created_at: 2026-11-23T13:07:05.226226308Z
---
# mk install default-runtime pruning

## Change
- `tasks.yaml` `install` now:
  - reads default language ids from `scripts/runtime-default-languages.txt`
  - passes `--languages rust,toml,yaml` (file-driven) to `scripts/install-tree-sitter-runtime.sh`
  - passes `--clean-grammars` so stale full-runtime grammar libraries are removed.
- `ee do runtime install` writes `grammars/.language-pack-installed` with installed grammar filenames on success and on already-installed early return.
- `scripts/install-tree-sitter-runtime.sh --clean-grammars`:
  - moves `grammars/` aside
  - restores marker-listed pack grammars
  - runs the build (default set only)
  - deletes the backup on success; restores the full backup on build failure.
- `mk install` therefore ends with default grammars plus user-activated language-pack grammars. Old pre-marker full-runtime grammars are pruned.
- README/CHANGELOG updated.

## Validation
- `mk validate`
- `mk plan install` shows the new command.
- `scripts/tests/test-install-tree-sitter-runtime.sh` covers clean prune, marker preservation, backup removal, and failure restore.
- `cargo fmt --check`
- `cargo clippy -p ee-cli --no-deps -- -D warnings`
- `cargo test --quiet -p ee-cli runtime_pack` (6 pass)
- Manual CLI install writes `.language-pack-installed`.
- Manual clean test preserves marker-listed python and removes unmarked rust-old files.

## Note
Pack installs made before this marker existed have no marker and will be pruned by the first `mk install`; reinstall them with `ee do runtime install <language>`.
