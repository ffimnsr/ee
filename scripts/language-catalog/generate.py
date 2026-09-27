#!/usr/bin/env python3
"""Generate ee Zed language catalog artifacts from fetched upstream docs.

Inputs:
  --docs-dir      directory containing <slug>.md Zed language docs
  --overrides     curated TOML (scripts/language-catalog/overrides.toml)
  --manifest      output JSON manifest path
  --queries-root  ee runtime/queries dir (query-dir presence check)
  --grammar-rust  output Rust catalog for xi-core-lib (optional)
  --lsp-rust      output Rust catalog for xi-lsp-lib (optional)
  --emit-rust     require Rust emission (sync.sh default)

Reads docs for "Tree-sitter:" / "Language Server:" bullets, merges overrides,
pins grammar repo HEAD revs via `git ls-remote`, and emits the manifest plus
checked-in Rust catalogs.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from datetime import datetime, timezone

OLD_MANIFEST = None

BUILTIN_LANGUAGES = {
    "bash", "c", "csharp", "cpp", "css", "elixir", "go", "haskell", "html",
    "java", "javascript", "json", "markdown", "php", "python", "ruby", "rust",
    "scala", "typescript", "yaml",
}

GRAMMAR_BULLET = re.compile(r"^- *Tree-sitter:\s*\[[^\]]*\]\(([^)]+)\)", re.MULTILINE)
LSP_BULLET = re.compile(r"^- *Language Server:\s*\[([^\]]*)\]\(([^)]+)\)", re.MULTILINE)


def die(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def load_old_manifest(path: str) -> dict | None:
    try:
        with open(path, "r", encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, json.JSONDecodeError):
        return None


def normalize_url(url: str) -> str:
    url = url.strip().rstrip("/")
    # Doc links sometimes point at a branch view: .../tree/master
    import re as _re
    url = _re.sub(r"/tree/[^/]+$", "", url)
    if url.endswith(".git"):
        url = url[:-4]
    return url


def first_bullet(text: str, pattern: re.Pattern) -> str | None:
    match = pattern.search(text)
    if not match:
        return None
    return normalize_url(match.group(1))


def pin_rev(url: str, fallback_rev: str) -> str:
    try:
        proc = subprocess.run(
            ["git", "ls-remote", url, "HEAD"],
            capture_output=True, text=True, timeout=30,
        )
        if proc.returncode == 0 and proc.stdout.strip():
            return proc.stdout.split()[0]
    except (OSError, subprocess.TimeoutExpired):
        pass
    return fallback_rev


def rust_str(value: str) -> str:
    return json.dumps(value, ensure_ascii=True)


def rust_str_list(values: list[str]) -> str:
    return "vec![" + ", ".join(f"{rust_str(v)}.to_string()" for v in values) + "]"


def metadata_expr(overlay: dict) -> str:
    """Emit a LanguageMetadata construction for the given overlay."""
    line = overlay.get("line_comment")
    line_expr = (
        f'LineCommentStyle::Token({rust_str(line)})' if line
        else "LineCommentStyle::Unsupported"
    )
    block = overlay.get("block_comment")
    if block and len(block) == 2:
        block_expr = (
            f"BlockCommentStyle::Tokens {{ open: {rust_str(block[0])}, "
            f"close: {rust_str(block[1])} }}"
        )
    else:
        block_expr = "BlockCommentStyle::Unsupported"
    indent = overlay.get("indentation")
    indent_expr = (
        "IndentationStrategy::TreeSitter" if indent == "treesitter"
        else "IndentationStrategy::Unsupported"
    )
    return (
        f"LanguageMetadata {{\n"
        f"            line_comment: {line_expr},\n"
        f"            block_comment: {block_expr},\n"
        f"            indentation: {indent_expr},\n"
        f"            unsupported_semantic_targets: &[],\n"
        f"        }}"
    )


def grammar_rust_fn(entries: list[dict]) -> str:
    lines = [
        "// GENERATED FILE - do not edit by hand.",
        "// Regenerate with scripts/language-catalog/sync.sh.",
        "",
        "use std::collections::BTreeSet;",
        "",
        "use crate::syntax::LanguageDefinition;",
        "use crate::tree_sitter_support::{",
        "    BlockCommentStyle, IndentationStrategy, LanguageMetadata, LineCommentStyle,",
        "};",
        "",
        "use super::types::{",
        "    RuntimeGrammarConfig, RuntimeGrammarGitSource, RuntimeGrammarSource,",
        "    RuntimeLanguageConfig, RuntimeQueryKind,",
        "};",
        "",
        "/// Builtin language definitions for Zed-documented languages.",
        "pub(crate) fn generated_catalog_language_definitions() -> Vec<LanguageDefinition> {",
        "    vec![",
    ]
    for entry in entries:
        lines.append(
            f"        LanguageDefinition {{\n"
            f"            name: {rust_str(entry['id'])}.into(),\n"
            f"            extensions: {rust_str_list(entry['file_types'])},\n"
            f"            filenames: {rust_str_list(entry.get('filenames', []))},\n"
            f"            globs: {rust_str_list(entry.get('globs', []))},\n"
            "            first_line_match: None,\n"
            f"            scope: {rust_str('source.' + entry['id'].replace('-', '_'))}.into(),\n"
            "            default_config: None,\n"
            "        },"
        )
    lines += [
        "    ]",
        "}",
        "",
        "/// Grammar source overrides for Zed-documented languages (git-pinned).",
        "pub(crate) fn generated_catalog_language_overrides() -> Vec<(String, RuntimeLanguageConfig)> {",
        "    let standard_and_ee = RuntimeQueryKind::STANDARD",
        "        .into_iter()",
        "        .chain(RuntimeQueryKind::EE_OWNED)",
        "        .collect::<BTreeSet<_>>();",
        "",
        "    vec![",
    ]
    for entry in sorted(entries, key=lambda e: e["id"]):
        query_language_line = ""
        if entry.get("query_language") and entry["query_language"] != entry["id"]:
            query_language_line = (
                f"                query_language: Some({rust_str(entry['query_language'])}.to_string()),\n"
            )
        lines.append(
            "        (\n"
            f"            {rust_str(entry['id'])}.to_string(),\n"
            "            RuntimeLanguageConfig {\n"
            f"                aliases: Some({rust_str_list(entry['aliases'])}),\n"
            + query_language_line
            + "                supported_query_kinds: Some(standard_and_ee.clone()),\n"
            "                grammar: Some(RuntimeGrammarConfig {\n"
            f"                    library: Some({rust_str(entry['library'])}.to_string()),\n"
            f"                    symbol: Some({rust_str(entry['symbol'])}.to_string()),\n"
            "                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {\n"
            f"                        url: {rust_str(entry['grammar_url'])}.to_string(),\n"
            f"                        rev: Some({rust_str(entry['rev'])}.to_string()),\n"
            "                        branch: None,\n"
            "                        tag: None,\n"
            "                    })),\n"
            "                }),\n"
            f"                metadata: Some({metadata_expr(entry)}),\n"
            "                ..RuntimeLanguageConfig::default()\n"
            "            },\n"
            "        ),"
        )
    lines += [
        "    ]",
        "}",
        "",
    ]
    return "\n".join(lines)


def lsp_rust_fn(servers: list[dict], attachments: list[dict]) -> str:
    lines = [
        "// GENERATED FILE - do not edit by hand.",
        "// Regenerate with scripts/language-catalog/sync.sh.",
        "",
        "use std::collections::BTreeMap;",
        "",
        "use crate::types::LanguageConfig;",
        "",
        "/// Bundled LSP servers for Zed-documented languages.",
        "pub(crate) fn bundled_catalog_servers() -> Vec<(String, LanguageConfig)> {",
        "    vec![",
    ]
    for server in sorted(servers, key=lambda s: s["id"]):
        lines.append(
            "        (\n"
            f"            {rust_str(server['id'])}.to_string(),\n"
            "            LanguageConfig {\n"
            f"                language_name: {rust_str(server['language_name'])}.to_string(),\n"
            f"                start_command: {rust_str(server['command'])}.to_string(),\n"
            f"                start_arguments: {rust_str_list(server['args'])},\n"
            f"                extensions: {rust_str_list(server['extensions'])},\n"
            f"                filenames: {rust_str_list(server.get('filenames', []))},\n"
            f"                supports_single_file: {str(server.get('supports_single_file', True)).lower()},\n"
            "                workspace_identifier: "
            + (
                f"Some({rust_str(server['workspace_identifier'])}.to_string())"
                if server.get("workspace_identifier")
                else "None"
            )
            + ",\n"
            "                env: BTreeMap::new(),\n"
            "                initialization_options: None,\n"
            "            },\n"
            "        ),"
        )
    lines += [
        "    ]",
        "}",
        "",
        "/// Language -> server attachment routing for Zed-documented languages.",
        "pub(crate) fn bundled_catalog_routing() -> Vec<(String, Vec<String>)> {",
        "    vec![",
    ]
    for server in sorted(servers, key=lambda s: s["id"]):
        lines.append(
            f"        ({rust_str(server['id'])}.to_string(), vec![{rust_str(server['id'])}.to_string()]),"
        )
    for attach in sorted(attachments, key=lambda a: a["language_id"]):
        lines.append(
            f"        ({rust_str(attach['language_id'])}.to_string(), vec![{rust_str(attach['server_id'])}.to_string()]),"
        )
    lines += [
        "    ]",
        "}",
        "",
    ]
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--docs-dir", required=True)
    parser.add_argument("--overrides", required=True)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--queries-root", required=True)
    parser.add_argument("--grammar-rust")
    parser.add_argument("--lsp-rust")
    parser.add_argument("--emit-rust", action="store_true")
    parser.add_argument("--source-ref", default="refs/heads/main")
    args = parser.parse_args()

    if args.emit_rust and (not args.grammar_rust or not args.lsp_rust):
        die("--emit-rust requires --grammar-rust and --lsp-rust")

    global OLD_MANIFEST
    OLD_MANIFEST = load_old_manifest(args.manifest)

    with open(args.overrides, "rb") as fh:
        overrides = tomllib.load(fh)
    lang_overrides = overrides.get("languages", {})
    server_overrides = overrides.get("servers", {})

    doc_slugs = sorted(
        name[:-3] for name in os.listdir(args.docs_dir) if name.endswith(".md")
    )
    all_slugs = sorted(set(doc_slugs) | set(lang_overrides.keys()))
    failed_docs = []
    languages: dict[str, dict] = {}
    old_langs = (OLD_MANIFEST or {}).get("languages", {}) if OLD_MANIFEST else {}

    for slug in all_slugs:
        path = os.path.join(args.docs_dir, f"{slug}.md")
        if os.path.exists(path):
            with open(path, "r", encoding="utf-8") as fh:
                text = fh.read()
        else:
            # Override-only language (no upstream doc, e.g. `just`).
            text = ""
        ov = lang_overrides.get(slug, {})

        record = {
            "id": slug,
            "doc": f"{slug}.md" if text else None,
            "deferred": ov.get("defer", False),
            "defer_reason": ov.get("reason"),
            "defer_lsp": ov.get("defer_lsp", False),
            "grammar_url": None,
            "rev": None,
            "library": None,
            "symbol": None,
            "query_language": ov.get("query_language", slug),
            "file_types": ov.get("file_types", [slug]),
            "filenames": ov.get("filenames", []),
            "globs": ov.get("globs", []),
            "aliases": ov.get("aliases", [slug]),
            "line_comment": ov.get("line_comment"),
            "block_comment": ov.get("block_comment"),
            "indentation": ov.get("indentation"),
            "attach_lsp": ov.get("attach_lsp"),
            "lsp_doc_name": None,
            "lsp_server": None,
            "queries_present": None,
            "status": "ok",
        }

        title_match = re.search(r"^title:\s*(.+)$", text, re.MULTILINE)
        record["display_name"] = (
            title_match.group(1).strip().strip('"')
            if title_match
            else slug.title()
        )

        grammar_bullet = first_bullet(text, GRAMMAR_BULLET)
        grammar_url = ov.get("grammar_url") or grammar_bullet
        if grammar_url and slug not in BUILTIN_LANGUAGES and not ov.get("defer"):
            record["grammar_url"] = normalize_url(grammar_url)
            repo_name = record["grammar_url"].rstrip("/").rsplit("/", 1)[-1]
            record["library"] = ov.get("library") or repo_name or f"tree-sitter-{slug}"
            default_symbol = "tree_sitter_" + (
                repo_name.removeprefix("tree-sitter-").replace("-", "_")
                if repo_name.startswith("tree-sitter-")
                else slug.replace("-", "_")
            )
            record["symbol"] = ov.get("symbol") or default_symbol

        lsp_bullet = LSP_BULLET.search(text)
        if lsp_bullet:
            record["lsp_doc_name"] = lsp_bullet.group(1).strip()

        queries_dir = os.path.join(
            args.queries_root, record["query_language"].replace("-", "_")
        )
        record["queries_present"] = os.path.isdir(queries_dir)

        languages[slug] = record

    # Resolve revs (fall back to previously pinned revs when offline).
    for record in languages.values():
        if not record["grammar_url"]:
            continue
        old = old_langs.get(record["id"], {})
        record["rev"] = pin_rev(record["grammar_url"], old.get("rev") or "")
        if not record["rev"]:
            record["status"] = "rev_unresolved"

    # Attach LSP servers and new-server emission.
    new_servers = []
    attachments = []
    for record in languages.values():
        if record.get("attach_lsp"):
            attachments.append(
                {"language_id": record["id"], "server_id": record["attach_lsp"]}
            )
            record["lsp_server"] = record["attach_lsp"]
    for server_id, server in sorted(server_overrides.items()):
        record = {
            "id": server_id,
            "language_name": server["language_name"],
            "command": server["command"],
            "args": server.get("args", []),
            "extensions": server.get("extensions", []),
            "filenames": server.get("filenames", []),
            "supports_single_file": server.get("supports_single_file", True),
            "workspace_identifier": server.get("workspace_identifier"),
        }
        new_servers.append(record)

    manifest = {
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "source_ref": args.source_ref,
        "doc_count": len(doc_slugs),
        "docs_failed": failed_docs,
        "grammar_entries": sum(
            1 for r in languages.values() if r.get("grammar_url") and not r.get("deferred")
        ),
        "deferred": sorted(
            r["id"] for r in languages.values() if r.get("deferred")
        ),
        "languages": {
            rid: {k: v for k, v in r.items() if k != "id"}
            for rid, r in languages.items()
            if not rid.startswith("__server__")
        },
        "servers": new_servers,
    }

    path_dir = os.path.dirname(args.manifest)
    if path_dir:
        os.makedirs(path_dir, exist_ok=True)
    with open(args.manifest, "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2, sort_keys=True)
        fh.write("\n")

    if args.emit_rust:
        grammar_entries = [
            r for r in languages.values()
            if r.get("grammar_url") and not r.get("deferred") and r.get("rev")
        ]
        for record in grammar_entries:
            record.setdefault("file_types", [record["id"]])
            record.setdefault("aliases", [record["id"]])
        with open(args.grammar_rust, "w", encoding="utf-8") as fh:
            fh.write(grammar_rust_fn(grammar_entries))
        attach_entries = []
        for rid, record in languages.items():
            if record.get("attach_lsp"):
                attach_entries.append(
                    {"language_id": rid, "server_id": record["attach_lsp"]}
                )
        with open(args.lsp_rust, "w", encoding="utf-8") as fh:
            fh.write(lsp_rust_fn(new_servers, attach_entries))

        print(
            f"emitted {len(grammar_entries)} grammar entries, "
            f"{len(new_servers)} lsp servers, {len(attach_entries)} attachments"
        )


if __name__ == "__main__":
    main()