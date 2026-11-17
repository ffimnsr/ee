#!/usr/bin/env python3
"""Generate the OpenCode per-model catalog table.

Inputs (all under ``crates/ee-agent-opencode``):

* ``catalog/models-dev.subset.json`` -- committed subset of the models.dev
  catalog for the ``opencode`` and ``opencode-go`` providers.
* ``catalog/sources.json`` -- curated retired ids, unsupported ids, and the
  legacy routes kept from the pre-catalog table until upstream re-lists them.

Output: ``src/catalog/generated.rs``.

Usage (from ``crates/ee-agent-opencode``):

    python3 scripts/gen_catalog.py            # regenerate
    python3 scripts/gen_catalog.py --check    # fail when the file is stale
    python3 scripts/gen_catalog.py --refresh-snapshot /path/to/snapshot.txt

The generator is deterministic: models are sorted by surface and id, and all
numbers are integers. ``--refresh-snapshot`` accepts the models.dev catalog
JSON (the same file opencode bundles as ``models-dev/snapshot.txt``).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CATALOG_DIR = ROOT / "catalog"
SUBSET = CATALOG_DIR / "models-dev.subset.json"
SOURCES = CATALOG_DIR / "sources.json"
OUTPUT = ROOT / "src" / "catalog" / "generated.rs"

SUPPORTED_APIS = ("responses", "messages", "chat-completions")
DIALECTS = {
    "responses": "D::OpenAiResponses",
    "messages": "D::AnthropicMessages",
    "chat-completions": "D::OpenAiChatCompletions",
}
SURFACES = {"zen": "S::Zen", "go": "S::Go"}
PROVIDER_IDS = {"opencode": "zen", "opencode-go": "go"}
UNSUPPORTED_DIALECTS = {
    "google": "UnsupportedDialect::GoogleGenerativeLanguage",
    "systemone": "UnsupportedDialect::SystemOne",
}
KEPT_MODEL_FIELDS = ("id", "type", "capabilities", "limit", "cost", "api")


def compact_model(model: dict) -> dict:
    capabilities = model.get("capabilities") or {}
    reasoning = capabilities.get("reasoning") or {}
    tools = capabilities.get("tools") or {}
    limit = model.get("limit") or {}
    cost = ((model.get("cost") or {}).get("text")) or {}
    input_cost = cost.get("input") or {}
    output_cost = cost.get("output") or {}
    cache = input_cost.get("cache") or {}
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
                    "cache": {
                        key: cache[key] for key in ("read", "write") if key in cache
                    },
                    "overrides": list(input_cost.get("overrides") or []),
                },
                "output": {
                    "price": output_cost.get("price"),
                    "overrides": list(output_cost.get("overrides") or []),
                },
            }
        },
        "api": {
            kind: {
                "sdk": (entry.get("sdk") or {}).get("opencodeai", ""),
                "base_url": entry.get("base_url", ""),
                "compatibility": entry.get("compatibility") or {},
            }
            for kind, entry in sorted((model.get("api") or {}).items())
        },
    }


def reshape(raw: dict) -> dict:
    providers: dict[str, dict] = {}
    for provider_id, provider in raw.items():
        if provider_id not in PROVIDER_IDS:
            continue
        models: dict[str, dict] = {}
        for model_id, model in sorted(provider.get("models", {}).items()):
            models[model_id] = compact_model(model)
        providers[provider_id] = {
            "id": provider.get("id", provider_id),
            "models": models,
        }
    return {
        "source": "https://models.dev/api.json",
        "providers": providers,
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


def efforts(model: dict) -> list[str]:
    reasoning = model.get("capabilities", {}).get("reasoning", {}) or {}
    values = reasoning.get("effort")
    return [str(item) for item in values] if isinstance(values, list) else []


def reasoning_literal(model: dict) -> str:
    reasoning = model.get("capabilities", {}).get("reasoning", {}) or {}
    if not reasoning.get("supported"):
        return "ReasoningSupport::UNSUPPORTED"
    toggle = "true" if reasoning.get("toggle") else "false"
    values = efforts(model)
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


def cost_literal(model: dict) -> str:
    text = (model.get("cost") or {}).get("text")
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


def tiers_literal(model: dict) -> str:
    """Renders context-tier price overrides, ascending by threshold."""
    text = (model.get("cost") or {}).get("text") or {}
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
        slot = tiers.setdefault(threshold, {"input": base_input, "read": base_read, "write": base_write})
        slot["input"] = micros(entry.get("price"))
        cache = entry.get("cache") or {}
        if cache.get("read") is not None:
            slot["read"] = micros(cache.get("read"))
        if cache.get("write") is not None:
            slot["write"] = micros(cache.get("write"))
    for entry in outputs.get("overrides") or []:
        threshold = _override_threshold(entry)
        slot = tiers.setdefault(threshold, {"input": base_input, "read": base_read, "write": base_write})
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


def _override_threshold(entry: dict) -> int:
    when = entry.get("when") or {}
    context = when.get("context") or {}
    return int(context.get("gt") or 0)


def _option_micros(value: object) -> str:
    return "None" if value is None else f"Some({value})"


def routes_literal(model: dict) -> str:
    api = model.get("api") or {}
    return (
        "ModelRoutes { chat: "
        f"{'true' if 'chat-completions' in api else 'false'}, "
        f"responses: {'true' if 'responses' in api else 'false'}, "
        f"messages: {'true' if 'messages' in api else 'false'}, "
        f"images: {'true' if 'images' in api else 'false'}, "
        f"systemone: {'true' if 'systemone' in api else 'false'} }}"
    )


def route_kind(model: dict) -> str | None:
    api = model.get("api") or {}
    for kind in SUPPORTED_APIS:
        if kind in api:
            return kind
    return None


def unsupported_kind(model: dict) -> str | None:
    api = model.get("api") or {}
    if "generate-content" in api:
        return "google"
    if "systemone" in api:
        return "systemone"
    return None


def sampling_literal(model: dict) -> str:
    """Renders the sampling declaration.

    models.dev omits a sampling flag when the protocol default applies, so an
    absent flag means "accepted"; only an explicit `false` rejects the knob.
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


def compaction_literal(surface: str, model_id: str, kind: str, api: dict, allow: set) -> str:
    """Renders the provider-native compaction capability.

    Upstream exposes explicit compaction only on Responses routes served by the
    OpenAI provider package (`@opencode/ai/providers/openai/responses`), which
    re-exports the OpenAI Responses route carrying `/responses/compact`. The
    curated allowlist can force a row on after a live probe; it is kept empty
    until a probe proves the gateway serves the endpoint.
    """
    forced = (surface, model_id) in allow
    sdk = api.get("sdk") or ""
    if isinstance(sdk, dict):
        sdk = sdk.get("opencodeai", "")
    derived = kind == "responses" and sdk.endswith("providers/openai/responses")
    return "true" if forced or derived else "false"


def model_literal(surface: str, model: dict, kind: str, compaction_allow: set) -> str:
    limit = model.get("limit") or {}
    context = int(limit.get("context") or 0)
    output = int(limit.get("output") or 0)
    capabilities = model.get("capabilities") or {}
    tools = capabilities.get("tools") or {}
    structured = bool(capabilities.get("structured_output"))
    api = (model.get("api") or {}).get(kind) or {}
    compatibility = api.get("compatibility") or {}
    freeform = tools.get("freeform") or []
    freeform_literal = ", ".join(rust(str(item)) for item in freeform)
    modalities = model.get("modalities") or {}
    inputs = ", ".join(rust(str(item)) for item in (modalities.get("input") or []))
    outputs = ", ".join(rust(str(item)) for item in (modalities.get("output") or []))
    reasoning_field = compatibility.get("reasoning_field")
    return (
        "CatalogModel { surface: "
        f"{SURFACES[surface]}, id: {rust(model['id'])}, api_id: {rust(model['id'])}, "
        f"kind: ModelKind::Chat, dialect: {DIALECTS[kind]}, "
        f"routes: {routes_literal(model)}, status: {rust(model.get('status') or '')}, "
        f"family: {rust(model.get('family') or '')}, "
        f"base_url: {SURFACES[surface]}.api_root(), "
        f"sdk: {rust(api.get('sdk') or '')}, "
        f"limits: Some(ModelLimits::new({context}, {output})), "
        f"modalities: Modalities {{ input: &[{inputs}], output: &[{outputs}] }}, "
        f"reasoning: {reasoning_literal(model)}, cost: {cost_literal(model)}, "
        f"structured_output: {'true' if structured else 'false'}, "
        f"tools_supported: {'true' if tools.get('supported') else 'false'}, "
        f"tools_strict: {'true' if tools.get('strict') else 'false'}, "
        f"freeform: &[{freeform_literal}], "
        f"sampling: {sampling_literal(model)}, "
        f"max_tokens_field: {rust(compatibility.get('max_tokens_field') or '')}, "
        f"reasoning_field: {'None' if reasoning_field is None else f'Some({rust(str(reasoning_field))})'}, "
        f"compaction: {compaction_literal(surface, model['id'], kind, api, compaction_allow)}, "
        "legacy: false },"
    )


def legacy_literal(surface: str, model_id: str, kind: str) -> str:
    return (
        "CatalogModel { surface: "
        f"{SURFACES[surface]}, id: {rust(model_id)}, api_id: {rust(model_id)}, "
        f"kind: ModelKind::Chat, dialect: {DIALECTS[kind]}, "
        f"routes: ModelRoutes::CHAT_ONLY, status: \"\", family: \"\", "
        f"base_url: {SURFACES[surface]}.api_root(), sdk: \"\", limits: None, "
        "modalities: Modalities::UNKNOWN, "
        "reasoning: ReasoningSupport::UNSUPPORTED, cost: None, "
        "structured_output: false, tools_supported: true, tools_strict: false, "
        "freeform: &[], sampling: SamplingSupport::ALL, "
        "max_tokens_field: \"\", reasoning_field: None, compaction: false, "
        "legacy: true },"
    )


def generate() -> str:
    subset_bytes = SUBSET.read_bytes()
    digest = hashlib.sha256(subset_bytes).hexdigest()
    subset = json.loads(subset_bytes)
    sources = json.loads(SOURCES.read_text())

    retired = {(surface, model_id) for surface, model_id, _ in sources["retired"]}
    unsupported = {(surface, model_id) for surface, model_id, _ in sources["unsupported"]}
    compaction_allow = {(surface, model_id) for surface, model_id in sources.get("compaction", [])}

    rows: list[str] = []
    present: set[tuple[str, str]] = set()
    # Snapshot models whose only API is a protocol we do not implement.
    generated_unsupported: list[tuple[str, str, str]] = []

    for provider_id in ("opencode", "opencode-go"):
        surface = PROVIDER_IDS[provider_id]
        models = subset.get("providers", {}).get(provider_id, {}).get("models", {})
        for model_id in sorted(models):
            key = (surface, model_id)
            if key in retired or key in unsupported:
                continue
            model = models[model_id]
            if model.get("type", "chat") != "chat":
                continue
            kind = route_kind(model)
            if kind is None:
                dialect = unsupported_kind(model)
                if dialect is not None:
                    generated_unsupported.append((surface, model_id, dialect))
                continue
            limit = model.get("limit") or {}
            if int(limit.get("context") or 0) <= 0 or int(limit.get("output") or 0) <= 0:
                continue
            present.add(key)
            rows.append(model_literal(surface, model, kind, compaction_allow))

    legacy_rows: list[str] = []
    for surface, model_id, kind in sorted(sources["legacy"]):
        key = (surface, model_id)
        if key in present or key in retired or key in unsupported:
            continue
        if kind not in DIALECTS or surface not in SURFACES:
            raise SystemExit(f"invalid legacy entry {surface} {model_id} {kind}")
        legacy_rows.append(legacy_literal(surface, model_id, kind))

    retired_rows = [
        "RetiredModel { surface: "
        f"{SURFACES[surface]}, model_id: {rust(model_id)}, deprecated_on: {rust(date)} }},"
        for surface, model_id, date in sorted(sources["retired"])
    ]
    unsupported_rows = [
        "UnsupportedModel { surface: "
        f"{SURFACES[surface]}, model_id: {rust(model_id)}, "
        f"dialect: {UNSUPPORTED_DIALECTS[dialect]} }},"
        for surface, model_id, dialect in sorted(
            {*[(s, m, d) for s, m, d in sources["unsupported"]], *generated_unsupported}
        )
    ]

    def table(items: list[str]) -> str:
        return "\n".join(f"    {item}" for item in items)

    return f"""//! @generated by `scripts/gen_catalog.py` -- do not edit by hand.
//!
//! Source: models.dev catalog subset for providers `opencode` and `opencode-go`
//! (`{subset.get("source", "https://models.dev/api.json")}`), committed as
//! `catalog/models-dev.subset.json`.
//!
//! Snapshot SHA-256: `{digest}`
//!
//! Regenerate from `crates/ee-agent-opencode`:
//! `python3 scripts/gen_catalog.py`; verify with `--check`.

use ee_agent_orchestrator::ModelLimits;

use crate::catalog::{{
    CatalogModel, Modalities, ModelCost, ModelKind, ModelRoutes, ReasoningSupport, RetiredModel,
    SamplingSupport, UnsupportedModel,
}};
use crate::routes::{{OpenCodeDialect as D, OpenCodeSurface as S, UnsupportedDialect}};

/// Upstream catalog source.
pub(crate) const SNAPSHOT_SOURCE: &str = "https://models.dev/api.json";
/// SHA-256 of the committed snapshot subset.
pub(crate) const SNAPSHOT_SHA256: &str =
    "{digest}";

/// Every routable model, sorted by surface and id.
#[rustfmt::skip]
pub(crate) const MODELS: &[CatalogModel] = &[
{table(rows)}
];

/// Routes retained from the docs-era table with no invented metadata.
#[rustfmt::skip]
pub(crate) const LEGACY: &[CatalogModel] = &[
{table(legacy_rows)}
];

/// Models documented as retired on their surface.
#[rustfmt::skip]
pub(crate) const RETIRED: &[RetiredModel] = &[
{table(retired_rows)}
];

/// Models whose protocol class ee does not implement yet.
#[rustfmt::skip]
pub(crate) const UNSUPPORTED: &[UnsupportedModel] = &[
{table(unsupported_rows)}
];
"""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--refresh-snapshot", type=pathlib.Path)
    args = parser.parse_args()

    if args.refresh_snapshot:
        raw = json.loads(args.refresh_snapshot.read_text())
        SUBSET.write_text(json.dumps(reshape(raw), indent=2, sort_keys=True) + "\n")

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
