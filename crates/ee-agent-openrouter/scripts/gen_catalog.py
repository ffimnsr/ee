#!/usr/bin/env python3
"""Generate the OpenRouter per-model catalog table.

Inputs (all under ``crates/ee-agent-openrouter``):

* ``catalog/models-dev.openrouter.json`` -- committed subset of the models.dev
  catalog for the ``openrouter`` provider (same source opencode bundles as
  ``packages/core/src/models-dev/snapshot.txt``).

Output: ``src/catalog/generated.rs``.

Usage (from ``crates/ee-agent-openrouter``):

    python3 scripts/gen_catalog.py            # regenerate
    python3 scripts/gen_catalog.py --check    # fail when the file is stale
    python3 scripts/gen_catalog.py --refresh-snapshot /path/to/snapshot.txt

The generator is deterministic: models are sorted by id and all numbers are
integers. The subset keeps only the fields the request path reads, so the
committed snapshot stays reviewable.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CATALOG_DIR = ROOT / "catalog"
SUBSET = CATALOG_DIR / "models-dev.openrouter.json"
OUTPUT = ROOT / "src" / "catalog" / "generated.rs"
PROVIDER_ID = "openrouter"


def compact_model(model: dict) -> dict:
    api = model.get("api") or {}
    return {
        "id": model.get("id"),
        "type": model.get("type", "chat"),
        "status": model.get("status") or "",
        "family": model.get("family") or "",
        "release_date": model.get("release_date") or "",
        "capabilities": model.get("capabilities") or {},
        "modalities": {
            "input": list((model.get("modalities") or {}).get("input") or []),
            "output": list((model.get("modalities") or {}).get("output") or []),
        },
        "limit": {
            "context": int((model.get("limit") or {}).get("context") or 0),
            "output": int((model.get("limit") or {}).get("output") or 0),
        },
        "cost": model.get("cost"),
        "api": {
            kind: {
                "base_url": entry.get("base_url", ""),
                "compatibility": entry.get("compatibility") or {},
                "capabilities": entry.get("capabilities") or {},
            }
            for kind, entry in sorted(api.items())
        },
    }


def reshape(raw: dict) -> dict:
    provider = raw.get(PROVIDER_ID)
    if provider is None:
        raise SystemExit(f"snapshot has no {PROVIDER_ID!r} provider")
    models = {
        model_id: compact_model(model)
        for model_id, model in sorted(provider.get("models", {}).items())
    }
    return {
        "source": "https://models.dev/api.json",
        "providers": {PROVIDER_ID: {"id": provider.get("id", PROVIDER_ID), "models": models}},
    }


def micros(value: object) -> int:
    if value is None:
        return 0
    if isinstance(value, dict):
        # models.dev allows one write price per TTL; keep the first only.
        return micros(next(iter(value.values()), 0))
    return int(round(float(value) * 1_000_000))


def rust(value: str) -> str:
    return json.dumps(value)


def _option_micros(value: object) -> str:
    return "None" if value is None else f"Some({value})"


def _override_threshold(entry: dict) -> int:
    return int((((entry.get("when") or {}).get("context") or {}).get("gt")) or 0)


def tiers_literal(model: dict) -> str:
    """Renders context-tier price overrides, ascending by threshold."""
    text = ((model.get("cost") or {}).get("text")) or {}
    inputs = text.get("input") or {}
    outputs = text.get("output") or {}
    base_input = micros(inputs.get("price"))
    base_output = micros(outputs.get("price"))
    base_cache = inputs.get("cache") or {}
    base_read = None if base_cache.get("read") is None else micros(base_cache.get("read"))
    base_write = None if base_cache.get("write") is None else micros(base_cache.get("write"))
    tiers: dict[int, dict[str, object]] = {}
    for entry in inputs.get("overrides") or []:
        threshold = _override_threshold(entry)
        slot = tiers.setdefault(
            threshold, {"input": base_input, "read": base_read, "write": base_write}
        )
        slot["input"] = micros(entry.get("price"))
        cache = entry.get("cache") or {}
        if cache.get("read") is not None:
            slot["read"] = micros(cache.get("read"))
        if cache.get("write") is not None:
            slot["write"] = micros(cache.get("write"))
    for entry in outputs.get("overrides") or []:
        threshold = _override_threshold(entry)
        slot = tiers.setdefault(
            threshold, {"input": base_input, "read": base_read, "write": base_write}
        )
        slot["output"] = micros(entry.get("price"))
    rendered = []
    for threshold in sorted(tiers):
        slot = tiers[threshold]
        rendered.append(
            "CostTier { input_context_above: "
            f"{threshold}, input_micros: {slot['input']}, "
            f"output_micros: {slot.get('output', base_output)}, "
            f"cache_read_micros: {_option_micros(slot['read'])}, "
            f"cache_write_micros: {_option_micros(slot['write'])} }}"
        )
    return ", ".join(rendered)


def cost_literal(model: dict) -> str:
    text = ((model.get("cost") or {}).get("text")) or None
    if not text:
        return "None"
    input_price = (text.get("input") or {}).get("price")
    output_price = (text.get("output") or {}).get("price")
    if input_price is None or output_price is None:
        return "None"
    cache = (text.get("input") or {}).get("cache") or {}
    cache_read = cache.get("read")
    cache_write = cache.get("write")
    return (
        "Some(ModelCost { input_micros: "
        f"{micros(input_price)}, output_micros: {micros(output_price)}, "
        f"cache_read_micros: {'None' if cache_read is None else f'Some({micros(cache_read)})'}, "
        f"cache_write_micros: {'None' if cache_write is None else f'Some({micros(cache_write)})'}, "
        f"tiers: &[{tiers_literal(model)}] }})"
    )


def reasoning_literal(model: dict) -> str:
    reasoning = (model.get("capabilities") or {}).get("reasoning") or {}
    if not reasoning.get("supported"):
        return "ReasoningSupport::UNSUPPORTED"
    toggle = "true" if reasoning.get("toggle") else "false"
    values = reasoning.get("effort")
    values = [str(item) for item in values] if isinstance(values, list) else []
    rendered = ", ".join(rust(item) for item in values)
    budget = reasoning.get("budget") or {}
    budget_min = budget.get("min")
    budget_max = budget.get("max")
    return (
        "ReasoningSupport { supported: true, toggle: "
        f"{toggle}, efforts: &[{rendered}], budget_min: "
        f"{'None' if budget_min is None else f'Some({int(budget_min)})'}, budget_max: "
        f"{'None' if budget_max is None else f'Some({int(budget_max)})'} }}"
    )


def routes_literal(model: dict) -> str:
    kinds = set((model.get("api") or {}).keys())
    return (
        "ModelRoutes { chat: "
        f"{'true' if 'chat-completions' in kinds else 'false'}, "
        f"responses: {'true' if 'responses' in kinds else 'false'}, "
        f"messages: {'true' if 'messages' in kinds else 'false'}, "
        f"images: {'true' if 'images' in kinds else 'false'}, "
        f"systemone: {'true' if 'systemone' in kinds else 'false'} }}"
    )


CACHE_FORMATS = ("anthropic", "alibaba")
TOOL_CHOICES = ("auto", "none", "tool", "required")
REASONING_EFFORTS = ("none", "minimal", "low", "medium", "high", "xhigh", "max")
STATUSES = ("", "alpha", "beta", "preview", "deprecated", "retired")
SERVICE_TIERS = ("auto", "default", "flex", "priority", "scale")
COMPATIBILITY_KEYS = (
    "reasoning_field",
    "require_reasoning",
    "max_tokens_field",
    "require_finish_reason",
    "require_assistant_after_tool",
    "supports_prompt_cache_key",
    "stream_usage",
    "cache_format",
    "supports_thinking_block_binding",
    "supports_effort_updates",
    "media_input",
)


def validate_model(model_id: str, model: dict) -> None:
    """Fails loudly on an unknown enum instead of silently skipping it."""
    kind = model.get("type", "chat")
    if kind not in ("chat", "decision", "image", "embedding"):
        raise SystemExit(f"{model_id}: unsupported catalog type {kind!r}")
    status = model.get("status") or ""
    if status not in STATUSES:
        raise SystemExit(f"{model_id}: unknown status {status!r}")
    capabilities = model.get("capabilities") or {}
    for effort in (capabilities.get("reasoning") or {}).get("effort") or []:
        if str(effort) not in REASONING_EFFORTS:
            raise SystemExit(f"{model_id}: unknown reasoning effort {effort!r}")
    cache = capabilities.get("cache") or {}
    for key in cache:
        if key not in ("implicit", "explicit"):
            raise SystemExit(f"{model_id}: unknown cache key {key!r}")
    for tier in capabilities.get("service_tier") or []:
        if str(tier) not in SERVICE_TIERS:
            raise SystemExit(f"{model_id}: unknown service tier {tier!r}")
    for entry in (model.get("api") or {}).values():
        for key, value in (entry.get("compatibility") or {}).items():
            if key not in COMPATIBILITY_KEYS:
                raise SystemExit(f"{model_id}: unknown compatibility key {key!r}")
            if key == "cache_format" and value not in CACHE_FORMATS:
                raise SystemExit(f"{model_id}: unknown cache format {value!r}")
        tools = ((entry.get("capabilities") or {}).get("tools")) or {}
        for choice in tools.get("choice") or []:
            if str(choice) not in TOOL_CHOICES:
                raise SystemExit(f"{model_id}: unknown tool choice {choice!r}")


def _cache_format(compatibility: dict) -> str:
    """Marker format for one row.

    The snapshot is authoritative: a row that needs the Alibaba marker shape
    must declare `cache_format: alibaba`. There is deliberately no vendor or
    model-id fallback, because that is the heuristic the capability matrix
    exists to remove. The Qwen placeholder capability below names Alibaba
    explicitly instead.
    """
    declared = compatibility.get("cache_format")
    if declared:
        return declared
    return "anthropic"


def _is_qwen_family(family: str) -> bool:
    """Matches the reference cache-policy branch for Qwen-family routes.

    The models.dev snapshot does not declare a cache capability for OpenRouter
    Qwen routes, but the reference agent still places breakpoints on them. The
    `family` field is authoritative and covers every shipped Qwen row
    (`qwen`, `qwen3.5`, `qwen3.6`, `qwen3.8-max`); no id-prefix fallback is
    needed, so a missing family simply means the row is not Qwen.
    """
    return family.lower().startswith("qwen")


def qwen_cache_literal() -> str:
    """System + tail placement, never tool definitions.

    `max_breakpoints` stays at the global cap so the policy's own marks are
    never dropped; the Qwen shape simply never marks tools.
    """
    return (
        "CacheCapability { implicit: false, explicit: true, max_breakpoints: 4, "
        "targets: CacheTargets { tools: false, system: true, messages: true }, "
        "min_tokens: None, ttl: &[], "
        "format: CacheFormatId::Alibaba, placement: CachePlacement::SystemAndTail }"
    )


def cache_literal(
    model_id: str, family: str, cache: dict, compatibility: dict
) -> str:
    if not cache:
        return qwen_cache_literal() if _is_qwen_family(family) else "CacheCapability::NONE"
    explicit = cache.get("explicit")
    implicit = cache.get("implicit")
    explicit_settings = explicit if isinstance(explicit, dict) else {}
    implicit_settings = implicit if isinstance(implicit, dict) else {}
    format_id = _cache_format(compatibility)
    targets = explicit_settings.get("targets")
    if targets is None:
        targets = (
            ["system", "messages"]
            if format_id == "alibaba"
            else ["tools", "system", "messages"]
        )
    ttl = list(explicit_settings.get("ttl") or implicit_settings.get("ttl") or [])
    min_tokens = explicit_settings.get("min_tokens", implicit_settings.get("min_tokens"))
    placement = "None"
    if explicit is not None:
        placement = "SystemAndTail" if format_id == "alibaba" else "Auto"
    rendered_ttl = ", ".join(rust(str(item)) for item in ttl)
    return (
        "CacheCapability { implicit: "
        f"{'true' if implicit is not None else 'false'}, "
        f"explicit: {'true' if explicit is not None else 'false'}, "
        f"max_breakpoints: {int(explicit_settings.get('max_breakpoints') or 0)}, "
        "targets: CacheTargets { "
        f"tools: {'true' if 'tools' in targets else 'false'}, "
        f"system: {'true' if 'system' in targets else 'false'}, "
        f"messages: {'true' if 'messages' in targets else 'false'} }}, "
        f"min_tokens: {'None' if min_tokens is None else f'Some({int(min_tokens)})'}, "
        f"ttl: &[{rendered_ttl}], "
        f"format: CacheFormatId::{format_id.capitalize()}, "
        f"placement: CachePlacement::{placement} }}"
    )


def capabilities_literal(model: dict) -> str:
    capabilities = model.get("capabilities") or {}
    cache = capabilities.get("cache") or {}
    api = model.get("api") or {}
    # The chat-completions entry describes the tool plane the request path uses;
    # fall back to the first declared kind when a model has no chat route.
    entry = api.get("chat-completions") or next(iter(api.values()), {})
    compatibility = entry.get("compatibility") or {}
    api_capabilities = entry.get("capabilities") or {}
    tools = api_capabilities.get("tools") or {}
    choices = ", ".join(rust(str(item)) for item in tools.get("choice") or [])
    return (
        "ModelCapabilities { tool_choices: &["
        f"{choices}], "
        f"tool_updates: {'true' if tools.get('updates') else 'false'}, "
        f"cache: {cache_literal(model['id'], model.get('family') or '', cache, compatibility)}, "
        # The snapshot declares no provider state for this provider, so the
        # declared routes decide: chat/responses stream usage and responses
        # exposes store.
        f"store: {'true' if 'responses' in api else 'false'}, "
        f"stream_usage: {'true' if 'chat-completions' in api or 'responses' in api else 'false'} }}"
    )


def sampling_literal(model: dict) -> str:
    """Renders the sampling declaration from the catalog flags.

    An absent flag means the protocol default applies, so the knob is accepted;
    only an explicit `false` rejects it.
    """
    capabilities = model.get("capabilities") or {}

    def flag(camel: str, snake: str) -> str:
        value = capabilities.get(camel, capabilities.get(snake))
        return "false" if value is False else "true"

    return (
        "SamplingSupport { temperature: "
        f"{flag('temperature', 'temperature')}, top_p: {flag('topP', 'top_p')}, "
        f"top_k: {flag('topK', 'top_k')} }}"
    )


def _default_max_tokens_field(model_id: str) -> str:
    """Vendors whose OpenAI-compatible surface still requires `max_tokens`.

    This mapping lives in the generator so the request path reads data instead
    of matching model-id substrings at runtime.
    """
    vendor = model_id.lstrip("~").split("/")[0].lower()
    requires = (
        "deepseek",
        "moonshotai",
        "togetherai",
        "zai",
        "z-ai",
        "zhipuai",
        "nvidia",
        "cerebras",
        "chutes",
        "cloudflare",
    )
    return "max_tokens" if any(vendor.startswith(name) for name in requires) else "max_completion_tokens"


def model_literal(model: dict) -> str:
    kind = model.get("type", "chat")
    limit = model.get("limit") or {}
    context = int(limit.get("context") or 0)
    output = int(limit.get("output") or 0)
    limits = (
        f"Some(ModelLimits::new({context}, {output}))"
        if kind == "chat" and context > 0 and output > 0
        else "None"
    )
    capabilities = model.get("capabilities") or {}
    tools = capabilities.get("tools") or {}
    compatibility = (model.get("api") or {}).get("compatibility") or {}
    freeform = ", ".join(rust(str(item)) for item in (tools.get("freeform") or []))
    modalities = model.get("modalities") or {}
    inputs = ", ".join(rust(str(item)) for item in (modalities.get("input") or []))
    outputs = ", ".join(rust(str(item)) for item in (modalities.get("output") or []))
    reasoning_field = compatibility.get("reasoning_field")
    api = model.get("api") or {}
    base_url = (api.get("chat-completions") or {}).get("base_url", "")
    max_tokens_field = compatibility.get("max_tokens_field") or (
        _default_max_tokens_field(model["id"])
        if "chat-completions" in api
        else ""
    )
    return (
        "CatalogModel { id: "
        f"{rust(model['id'])}, kind: {kind_literal(kind)}, "
        f"routes: {routes_literal(model)}, status: {rust(model.get('status') or '')}, "
        f"family: {rust(model.get('family') or '')}, "
        f"release_date: {rust(model.get('release_date') or '')}, "
        f"capabilities: {capabilities_literal(model)}, "
        f"base_url: {rust(base_url)}, "
        f"limits: {limits}, "
        f"modalities: Modalities {{ input: &[{inputs}], output: &[{outputs}] }}, "
        f"reasoning: {reasoning_literal(model)}, cost: {cost_literal(model)}, "
        f"structured_output: {'true' if capabilities.get('structured_output') else 'false'}, "
        f"tools_supported: {'true' if tools.get('supported') else 'false'}, "
        f"tools_strict: {'true' if tools.get('strict') else 'false'}, "
        f"freeform: &[{freeform}], "
        f"sampling: {sampling_literal(model)}, "
        f"max_tokens_field: {rust(max_tokens_field)}, "
        f"reasoning_field: {'None' if reasoning_field is None else f'Some({rust(str(reasoning_field))})'}, "
        "compaction: false },"
    )


def kind_literal(kind: str) -> str:
    match kind:
        case "chat":
            return "ModelKind::Chat"
        case "decision":
            return "ModelKind::Decision"
        case "image":
            return "ModelKind::Image"
        case "embedding":
            return "ModelKind::Embedding"
        case other:
            raise SystemExit(f"unsupported catalog type {other!r}")


def generate() -> str:
    subset_bytes = SUBSET.read_bytes()
    digest = hashlib.sha256(subset_bytes).hexdigest()
    subset = json.loads(subset_bytes)
    models = subset.get("providers", {}).get(PROVIDER_ID, {}).get("models", {})
    for model_id in sorted(models):
        validate_model(model_id, models[model_id])
    rows = [model_literal(models[model_id]) for model_id in sorted(models)]
    source = subset.get("source", "https://models.dev/api.json")
    return f"""//! @generated by `scripts/gen_catalog.py` -- do not edit by hand.
//!
//! Source: models.dev catalog subset for provider `{PROVIDER_ID}`
//! (`{source}`), committed as `catalog/models-dev.openrouter.json`.
//!
//! Snapshot SHA-256: `{digest}`
//!
//! Regenerate from `crates/ee-agent-openrouter`:
//! `python3 scripts/gen_catalog.py`; verify with `--check`.

use ee_agent_orchestrator::{{
    CacheCapability, CacheFormatId, CachePlacement, CacheTargets, CostTier, Modalities,
    ModelCapabilities, ModelCost, ModelKind, ModelLimits, ModelRoutes, ReasoningSupport,
    SamplingSupport,
}};

use crate::catalog::CatalogModel;

/// Upstream catalog source.
pub(crate) const SNAPSHOT_SOURCE: &str = "{source}";
/// SHA-256 of the committed snapshot subset.
pub(crate) const SNAPSHOT_SHA256: &str =
    "{digest}";

/// Every catalog model, sorted by id.
#[rustfmt::skip]
pub(crate) const MODELS: &[CatalogModel] = &[
{chr(10).join(f"    {row}" for row in rows)}
];
"""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--refresh-snapshot", type=pathlib.Path)
    args = parser.parse_args()

    if args.refresh_snapshot:
        raw = json.loads(args.refresh_snapshot.read_text())
        SUBSET.write_text(
            json.dumps(reshape(raw), sort_keys=True, separators=(",", ":")) + "\n"
        )

    rendered = generate()
    if args.check:
        current = OUTPUT.read_text() if OUTPUT.exists() else ""
        if current != rendered:
            print(f"{OUTPUT} is stale; run scripts/gen_catalog.py", file=sys.stderr)
            return 1
        return 0
    OUTPUT.write_text(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
