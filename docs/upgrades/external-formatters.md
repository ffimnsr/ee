# External Language Formatters — Design + Plan

Status: **design, pending review** · Ask: `shfmt` on `bash`/`sh`, arbitrary external formatters per language.

## 1. Current state (verified)

- Format pipeline: `format` command → backend `format_document` edit → `xi-lsp-lib` sends `textDocument/formatting` → edits applied via `LspResponse::Formatting` → `apply_named_edits`.
- **No external formatter support exists.** Formatting is LSP-only; languages without a formatter-capable server (sh/bash, diff, make, sql, rst…) cannot format.
- Formatting semantics are backend-owned (RULE.md: "Plugin and LSP integration: … formatting"); editor config loading is frontend-owned.

## 2. Goal

```toml
[formatters.shfmt]
command = "shfmt"
args = ["-"]

[languages.shell]
formatter = "shfmt"   # attach; one formatter can serve many languages

[languages.markdown]
formatter = false     # disable formatting
```

- `[formatters.<id>]` defines a shared formatter (command/args/bounds), mirroring `[lsp.servers.<id>]`.
- `[languages.<id>].formatter` attaches a definition or disables formatting, mirroring the `[languages.<id>].lsp` attachment field.
- Absent attachment = LSP `textDocument/formatting` (current behavior, default).

- `mode = "external"`: run `command args`, write buffer text on stdin, read formatted text from stdout, diff → edits.
- `mode = "language_server"` (default): today's LSP behavior.
- `mode = "none"`: disable formatting for a language.

## 3. Design

### 3.1 Config (frontend-owned, layered like LSP)

- `EeToml` gains a top-level `[formatters.<id>]` table (implemented as a sibling of `[lsp.servers.<id>]`): `{ command, args, timeout_ms, max_output_bytes }`. One definition is shared across languages.
- Attachment rides the existing `[languages.<id>]` table as `formatter` (new `RuntimeLanguageConfig.formatter` field of untagged enum `FormatterAttachment { Id(String), Disabled(bool) }`, same shape as the `lsp` attachment field): `formatter = "shfmt"` or `formatter = false`.
- Resolved through the same layers (`/etc`, XDG, `~/.ee.toml`, ancestor `.ee.toml`); scalars merge per layer, arrays replace. Attachments to undefined formatter ids warn and drop (mirrors unknown `lsp` server ids).

### 3.2 Execution (backend, `xi-lsp-lib`)

- `format_document` dispatch: attachment mapping says `Id(id)` -> external runner; `Disabled` -> status item; absent -> language server path.
- External path:
  1. look up binary on `PATH` (fail closed with clear status item, mirror `spawn_failure_status`)
  2. spawn `command args`, `stdin` = current buffer rope, `cwd` = workspace root (trusted), `env` inherited
  3. timeout (default 5s) + stdout byte cap (default 8 MiB) + UTF-8 validation — kill on breach
  4. diff formatted vs current buffer (rope diff), produce `TextEdit`s
  5. push into existing `LspResponse::Formatting` result queue → `apply_named_edits` (undo-friendly, revision-safe)
- No shell interpolation, no network, no temp files (stdin/stdout only). Fail closed on any error; editing never blocked.

### 3.3 RULE.md boundary

- Frontend resolves formatter config from layers and sends resolved values (config loading is frontend-owned; no removal of violation).
- Backend owns execution semantics + edit application (plugin integration is backend-owned).
- No changes to core edit protocol; formatting stays a named-edit flow.

## 4. Plan

1. **Phase 1 — config**: `EeToml.languages.<id>.formatter` schema (`raw.rs`), frontend merge + `validate` + `to_plugin_config` extension; schema regen (`ee do schema generate`); config layer tests (override precedence, `none` disables).
2. **Phase 2 — plugin**: `Config.formatters` type in `xi-lsp-lib` (`types.rs`), config update wiring, `format_document` dispatch + external runner (spawn/timeout/cap/diff), status item on missing binary/timeout.
3. **Phase 3 — tests**: fake-formatter script fixtures (existing `tests/support` style): happy path edits, empty diff no-op, missing binary status, timeout kill, UTF-8 rejection; LSP-format regression for unconfigured languages.
4. **Phase 4 — docs**: README + CHANGELOG; `shfmt` example for bash/sh in README.

## 5. Decisions / open points

- `stdin = true` fixed for v1 (bytes on stdin, stdout replaces). `filename`-argument mode (some formatters need `--write` on a path) deferred.
- Format-on-save: out of scope (no save hooks today).
- Default formatter catalog (e.g. shfmt for bash, black for python) deferred — config-only v1.
- Language without formatter + no LSP: `format` shows status "no formatter configured for `<id>`" instead of silent no-op.