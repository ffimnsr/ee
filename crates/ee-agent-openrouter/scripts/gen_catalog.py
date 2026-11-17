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
    capabilities = model.get("capabilities") or {}
    reasoning = capabilities.get("reasoning") or {}
    tools = capabilities.get("tools") or {}
    limit = model.get("limit") or {}
    cost = ((model.get("cost") or {}).get("text")) or {}
    input_cost = cost.get("input") or {}
    output_cost = cost.get("output") or {}
    cache = input_cost.get("cache") or {}
    api = model.get("api") or {}
    compatibility = (api.get("chat-completions") or {}).get("compatibility") or {}
    return {
        "id": model.get("id"),
        "type": model.get("type", "chat"),
        "status": model.get("status") or "",
        "family": model.get("family") or "",
        "capabilities": {
            "tools": {
                "supported": bool(tools.get("supported")),
                "strict": bool(tools.get("strict")),
                "freeform": list(tools.get("freeform") or []),
            },
            "reasoning": {
                key: reasoning[key]
                for key in ("supported", "toggle", "effort", "budget")
                if key in reasoning
            },
            "structured_output": bool(capabilities.get("structured_output")),
            "sampling": {
                key: capabilities[key]
                for key in ("temperature", "topP", "topK")
                if key in capabilities
            },
        },
        "modalities": {
            "input": list((model.get("modalities") or {}).get("input") or []),
            "output": list((model.get("modalities") or {}).get("output") or []),
        },
        "limit": {
            "context": int(limit.get("context") or 0),
            "output": int(limit.get("output") or 0),
        },
        "cost": {
            "text": {
                "input": {
                    "price": input_cost.get("price"),
                    "cache": {key: cache[key] for key in ("read", "write") if key in cache},
                    "overrides": list(input_cost.get("overrides") or []),
                },
                "output": {
                    "price": output_cost.get("price"),
                    "overrides": list(output_cost.get("overrides") or []),
                },
            }
        }
        if cost
        else None,
        "api": {
            "kinds": sorted(api),
            "base_url": (api.get("chat-completions") or {}).get("base_url", ""),
            "compatibility": compatibility,
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
    kinds = set((model.get("api") or {}).get("kinds") or [])
    return (
        "ModelRoutes { chat: "
        f"{'true' if 'chat-completions' in kinds else 'false'}, "
        f"responses: {'true' if 'responses' in kinds else 'false'}, "
        f"messages: {'true' if 'messages' in kinds else 'false'}, "
        f"images: {'true' if 'images' in kinds else 'false'}, "
        f"systemone: {'true' if 'systemone' in kinds else 'false'} }}"
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
    return (
        "CatalogModel { id: "
        f"{rust(model['id'])}, kind: {kind_literal(kind)}, "
        f"routes: {routes_literal(model)}, status: {rust(model.get('status') or '')}, "
        f"family: {rust(model.get('family') or '')}, "
        f"base_url: {rust((model.get('api') or {}).get('base_url') or '')}, "
        f"limits: {limits}, "
        f"modalities: Modalities {{ input: &[{inputs}], output: &[{outputs}] }}, "
        f"reasoning: {reasoning_literal(model)}, cost: {cost_literal(model)}, "
        f"structured_output: {'true' if capabilities.get('structured_output') else 'false'}, "
        f"tools_supported: {'true' if tools.get('supported') else 'false'}, "
        f"tools_strict: {'true' if tools.get('strict') else 'false'}, "
        f"freeform: &[{freeform}], "
        f"sampling: {sampling_literal(model)}, "
        f"max_tokens_field: {rust(compatibility.get('max_tokens_field') or '')}, "
        f"reasoning_field: {'None' if reasoning_field is None else f'Some({rust(str(reasoning_field))})'} }},"
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
    CostTier, Modalities, ModelCost, ModelKind, ModelLimits, ModelRoutes, ReasoningSupport,
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
