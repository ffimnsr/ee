//! Provider-neutral model metadata: limits, cost, modalities, reasoning
//! variants, and media bounds.
//!
//! Two provider crates (`ee-agent-opencode`, `ee-agent-openrouter`) route
//! requests from a generated catalog. The *shape* of that metadata, the
//! reasoning-variant derivation, the per-turn cost computation, and the media
//! guards are protocol properties rather than provider properties, so they live
//! here and each provider keeps only its own row table.
//!
//! Nothing in this module performs I/O, reads a credential, or selects an
//! endpoint: it turns already-resolved catalog values into request decisions.

use crate::model::{ModelContent, ModelMessage, ModelResponse, ModelUsage};

/// What a catalog entry produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// A text chat model.
    Chat,
    /// A typed decision model (OpenCode System One, OpenRouter decisions).
    Decision,
    /// An image-generation model.
    Image,
    /// An embedding model.
    Embedding,
}

impl ModelKind {
    /// Stable lowercase label used by listings and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Decision => "decision",
            Self::Image => "image",
            Self::Embedding => "embedding",
        }
    }

    /// Parses a catalog `type` value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Self::Chat),
            "decision" => Some(Self::Decision),
            "image" => Some(Self::Image),
            "embedding" => Some(Self::Embedding),
            _ => None,
        }
    }
}

/// Protocol routes a catalog entry advertises.
///
/// A provider may know several routes for the same model id; the caller selects
/// one explicitly and never infers it from the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModelRoutes {
    /// OpenAI-compatible Chat Completions.
    pub chat: bool,
    /// OpenAI Responses.
    pub responses: bool,
    /// Anthropic Messages.
    pub messages: bool,
    /// Image generation.
    pub images: bool,
    /// System One decision protocol.
    pub systemone: bool,
}

impl ModelRoutes {
    /// Only the chat-completions route.
    pub const CHAT_ONLY: Self =
        Self { chat: true, responses: false, messages: false, images: false, systemone: false };

    /// Whether no route is declared.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !(self.chat || self.responses || self.messages || self.images || self.systemone)
    }
}

/// One context-tier price override.
///
/// The override applies when the request's input-token count is above
/// `input_context_above`; the highest matching threshold wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostTier {
    /// Input-token threshold the tier starts above.
    pub input_context_above: u64,
    /// Input price per million tokens, in integer micro-dollars.
    pub input_micros: u64,
    /// Output price per million tokens, in integer micro-dollars.
    pub output_micros: u64,
    /// Cached-input read price per million tokens, when priced separately.
    pub cache_read_micros: Option<u64>,
    /// Cache-write price per million tokens, when priced separately.
    pub cache_write_micros: Option<u64>,
}

/// Monetary cost per million tokens, in integer micro-dollars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCost {
    /// Input price per million tokens.
    pub input_micros: u64,
    /// Output price per million tokens.
    pub output_micros: u64,
    /// Cached-input read price per million tokens, when priced separately.
    pub cache_read_micros: Option<u64>,
    /// Cache-write price per million tokens, when priced separately.
    pub cache_write_micros: Option<u64>,
    /// Context-length price overrides, ordered by threshold ascending.
    pub tiers: &'static [CostTier],
}

/// Token counts a price applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenCounts {
    /// Prompt tokens reported by the provider.
    pub input: u64,
    /// Completion tokens reported by the provider.
    pub output: u64,
    /// Prompt tokens served from the provider cache.
    pub cached_input: u64,
    /// Prompt tokens written to the provider cache.
    pub cache_write: u64,
}

impl TokenCounts {
    /// Builds token counts from normalized usage, treating missing fields as
    /// zero for pricing only.
    #[must_use]
    pub fn from_usage(usage: &ModelUsage) -> Self {
        let input = usage.input_tokens.unwrap_or(0) as u64;
        let cached_input = usage.cached_input_tokens.unwrap_or(0) as u64;
        Self {
            input,
            output: usage.output_tokens.unwrap_or(0) as u64,
            cached_input,
            cache_write: usage.cache_creation_input_tokens.unwrap_or(0) as u64,
        }
    }
}

impl ModelCost {
    /// Returns the tier that applies to `input_tokens`, if any.
    ///
    /// Tier thresholds are `>` comparisons, matching the upstream catalog.
    #[must_use]
    pub fn tier_for(&self, input_tokens: u64) -> Option<&CostTier> {
        self.tiers.iter().rfind(|tier| input_tokens > tier.input_context_above)
    }

    /// Computes the price of one turn in integer micro-dollars.
    ///
    /// Cached input is billed at the cache-read price when one is declared and
    /// at the input price otherwise; cache-write tokens are billed at the
    /// cache-write price when declared. A zero price is a real price (free
    /// models), so this never turns a known price into `None`.
    #[must_use]
    pub fn cost_micros(&self, tokens: TokenCounts) -> u64 {
        let tier = self.tier_for(tokens.input);
        let input_micros = tier.map_or(self.input_micros, |tier| tier.input_micros);
        let output_micros = tier.map_or(self.output_micros, |tier| tier.output_micros);
        let cache_read_micros = tier
            .and_then(|tier| tier.cache_read_micros)
            .or(self.cache_read_micros)
            .unwrap_or(input_micros);
        let cache_write_micros = tier
            .and_then(|tier| tier.cache_write_micros)
            .or(self.cache_write_micros)
            .unwrap_or(input_micros);
        let cached = tokens.cached_input.min(tokens.input);
        let write = tokens.cache_write.min(tokens.input.saturating_sub(cached));
        let uncached = tokens.input.saturating_sub(cached).saturating_sub(write);
        let total = u128::from(uncached) * u128::from(input_micros)
            + u128::from(cached) * u128::from(cache_read_micros)
            + u128::from(write) * u128::from(cache_write_micros)
            + u128::from(tokens.output) * u128::from(output_micros);
        u64::try_from(total / 1_000_000).unwrap_or(u64::MAX)
    }
}

/// Input/output modalities a model documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modalities {
    /// Accepted input MIME types (for example `text`, `image/png`, `application/pdf`).
    pub input: &'static [&'static str],
    /// Produced output MIME types.
    pub output: &'static [&'static str],
}

impl Modalities {
    /// No modality metadata; gates are skipped rather than guessed.
    pub const UNKNOWN: Self = Self { input: &[], output: &[] };

    /// Whether the model declares input for `mime`.
    ///
    /// A catalog entry may name a whole family (`image`) or an exact type
    /// (`image/png`); both forms accept a concrete `image/png`.
    #[must_use]
    pub fn accepts(&self, mime: &str) -> bool {
        if self.input.is_empty() {
            return true;
        }
        let mime = mime.trim().to_ascii_lowercase();
        let family = mime.split('/').next().unwrap_or_default();
        self.input.iter().any(|declared| {
            let declared = declared.to_ascii_lowercase();
            declared == mime
                || declared == family
                || declared.strip_suffix("/*").is_some_and(|prefix| prefix == family)
        })
    }

    /// Bounded human-readable declaration for diagnostics.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.input.is_empty() {
            String::from("no declared input modalities")
        } else {
            self.input.join(", ")
        }
    }
}

/// Sampling knobs a model documents.
///
/// The reference catalog treats an absent flag as "the protocol default applies"
/// (`packages/ai/src/schema/options.ts`), so a catalog row that declares nothing
/// accepts the knob. A row that explicitly declares `false` rejects it, which is
/// what the provider configuration validates before a request exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplingSupport {
    /// Whether `temperature` is accepted.
    pub temperature: bool,
    /// Whether `top_p` is accepted.
    pub top_p: bool,
    /// Whether `top_k` is accepted.
    pub top_k: bool,
}

impl SamplingSupport {
    /// Every knob is accepted; used for rows that declare no sampling metadata.
    pub const ALL: Self = Self { temperature: true, top_p: true, top_k: true };

    /// Builds the declaration from the catalog's optional flags, defaulting each
    /// unset flag to the protocol default (`true`).
    #[must_use]
    pub const fn declared(
        temperature: Option<bool>,
        top_p: Option<bool>,
        top_k: Option<bool>,
    ) -> Self {
        Self {
            temperature: !matches!(temperature, Some(false)),
            top_p: !matches!(top_p, Some(false)),
            top_k: !matches!(top_k, Some(false)),
        }
    }
}

impl Default for SamplingSupport {
    fn default() -> Self {
        Self::ALL
    }
}

/// Prompt planes a provider honours cache markers on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheTargets {
    /// Tool definitions.
    pub tools: bool,
    /// System prompt parts.
    pub system: bool,
    /// Conversation messages.
    pub messages: bool,
}

impl CacheTargets {
    /// Every plane.
    pub const ALL: Self = Self { tools: true, system: true, messages: true };
    /// No plane.
    pub const NONE: Self = Self { tools: false, system: false, messages: false };
    /// System and conversation messages, without tool definitions.
    pub const SYSTEM_AND_MESSAGES: Self = Self { tools: false, system: true, messages: true };
}

/// Cache marker syntax a route accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheFormatId {
    /// No marker syntax is declared.
    #[default]
    None,
    /// `cache_control: { type: "ephemeral" }` inline markers.
    Anthropic,
    /// Alibaba: tools cache with the system prompt, so tool breakpoints are
    /// skipped.
    Alibaba,
}

impl CacheFormatId {
    /// Parses the catalog's `cache_format` compatibility value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(Self::Anthropic),
            "alibaba" => Some(Self::Alibaba),
            _ => None,
        }
    }
}

/// Default automatic cache placement a provider family expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CachePlacement {
    /// Do not place automatic breakpoints.
    #[default]
    None,
    /// Last tool, first and last system part, and the conversation tail.
    Auto,
    /// System and conversation tail only; never tool definitions.
    SystemAndTail,
}

/// Prompt-cache capability declared by one catalog row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheCapability {
    /// The provider caches prefixes without any marker.
    pub implicit: bool,
    /// The provider honours inline cache markers.
    pub explicit: bool,
    /// Breakpoints accepted per request; `0` means the protocol default.
    pub max_breakpoints: usize,
    /// Planes that accept markers.
    pub targets: CacheTargets,
    /// Cacheable-prefix floor, when documented.
    pub min_tokens: Option<u64>,
    /// Accepted cache lifetimes in models.dev duration form (`5m`, `1h`, `24h`).
    pub ttl: &'static [&'static str],
    /// Marker syntax the route accepts.
    pub format: CacheFormatId,
    /// Automatic placement the provider family expects.
    pub placement: CachePlacement,
}

impl CacheCapability {
    /// No caching is declared.
    pub const NONE: Self = Self {
        implicit: false,
        explicit: false,
        max_breakpoints: 0,
        targets: CacheTargets::NONE,
        min_tokens: None,
        ttl: &[],
        format: CacheFormatId::None,
        placement: CachePlacement::None,
    };

    /// Whether the route can carry inline markers.
    #[must_use]
    pub const fn accepts_markers(&self) -> bool {
        self.explicit && (self.targets.tools || self.targets.system || self.targets.messages)
    }
}

/// Capability matrix declared by one catalog row.
///
/// Tool support, strict schemas, free-form formats, structured output, sampling,
/// limits, and cost stay on the provider row because they predate this matrix;
/// this struct carries the flags that replaced per-provider heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// `tool_choice` values the route honours; empty means the protocol default.
    pub tool_choices: &'static [&'static str],
    /// The route offers provider-executed search tools.
    pub tool_search: bool,
    /// The tool list may change between steps in one conversation.
    pub tool_updates: bool,
    /// Prompt-cache capability.
    pub cache: CacheCapability,
    /// Provider-side response state (`store` / `previous_response_id`).
    pub store: bool,
    /// Usage is reported in a streamed response.
    pub stream_usage: bool,
    /// The route accepts a request-level prompt cache key.
    pub prompt_cache_key: bool,
    /// Service tiers the route accepts; empty means none are declared.
    pub service_tiers: &'static [&'static str],
    /// The system prompt may change mid-session.
    pub system_updates: bool,
    /// Reasoning effort may be updated mid-session.
    pub reasoning_updates: bool,
    /// Every assistant turn must carry the reasoning field.
    pub require_reasoning: bool,
    /// A stream that ends without a finish reason is a failure.
    pub require_finish_reason: bool,
    /// An assistant message is required between a tool result and the next
    /// user message.
    pub require_assistant_after_tool: bool,
}

impl ModelCapabilities {
    /// Nothing is declared; callers fall back to protocol defaults.
    pub const UNKNOWN: Self = Self {
        tool_choices: &[],
        tool_search: false,
        tool_updates: false,
        cache: CacheCapability::NONE,
        store: false,
        stream_usage: false,
        prompt_cache_key: false,
        service_tiers: &[],
        system_updates: false,
        reasoning_updates: false,
        require_reasoning: false,
        require_finish_reason: false,
        require_assistant_after_tool: false,
    };
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

/// Reasoning controls a model documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningSupport {
    /// Whether the model accepts any reasoning control.
    pub supported: bool,
    /// Whether a plain on/off toggle exists.
    pub toggle: bool,
    /// Effort values the model accepts, in catalog order; empty means the
    /// protocol defaults apply when `supported` is true.
    pub efforts: &'static [&'static str],
    /// Minimum thinking budget in tokens, when budgets are supported.
    pub budget_min: Option<u64>,
    /// Maximum thinking budget in tokens, when budgets are supported.
    pub budget_max: Option<u64>,
}

impl ReasoningSupport {
    /// No reasoning control is available or known.
    pub const UNSUPPORTED: Self =
        Self { supported: false, toggle: false, efforts: &[], budget_min: None, budget_max: None };
}

/// Protocol family a reasoning overlay is lowered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningProtocol {
    /// OpenAI-compatible Chat Completions (`reasoning_effort`, `reasoning`).
    OpenAiChat,
    /// OpenAI Responses (`reasoning.effort`).
    OpenAiResponses,
    /// Anthropic Messages (`thinking.budget_tokens`).
    AnthropicMessages,
}

/// One selectable reasoning variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningVariant {
    /// Stable variant id used by `model#variant`.
    pub id: &'static str,
    /// Effort string, when the variant is an effort level.
    pub effort: Option<&'static str>,
    /// Thinking budget in tokens, when the variant is a budget.
    pub budget_tokens: Option<u64>,
    /// Whether the variant turns reasoning off.
    pub disabled: bool,
}

impl ReasoningVariant {
    /// Whether this variant describes no request change at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.effort.is_none() && self.budget_tokens.is_none() && !self.disabled
    }
}

/// Default Chat Completions effort levels when a model declares support but no
/// explicit values.
const DEFAULT_CHAT_EFFORTS: &[&str] = &["low", "medium", "high"];
/// Default Responses effort levels when a model declares support but no
/// explicit values.
const DEFAULT_RESPONSES_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];
/// Anthropic thinking budgets stay below the documented 32k output ceiling.
const ANTHROPIC_OUTPUT_TOKEN_MAX: u64 = 32_000;
/// Toggle-only Anthropic models get the reference 16k manual thinking budget.
const ANTHROPIC_MANUAL_THINKING_TOKENS: u64 = 16_000;

/// Derives the selectable reasoning variants for one model and protocol.
///
/// The order is stable and deduplicated: a disabled variant first when the
/// model supports toggling off, then effort variants in catalog order, then
/// budget variants (`high`, `max`). A model that declares no reasoning support
/// exposes no variants, so every `model#variant` selection fails at startup.
#[must_use]
pub fn reasoning_variants(
    support: ReasoningSupport,
    protocol: ReasoningProtocol,
    output_limit: Option<u64>,
) -> Vec<ReasoningVariant> {
    if !support.supported {
        return Vec::new();
    }
    let mut variants: Vec<ReasoningVariant> = Vec::new();
    match protocol {
        ReasoningProtocol::OpenAiChat | ReasoningProtocol::OpenAiResponses => {
            let defaults = if protocol == ReasoningProtocol::OpenAiChat {
                DEFAULT_CHAT_EFFORTS
            } else {
                DEFAULT_RESPONSES_EFFORTS
            };
            let efforts = if support.efforts.is_empty() { defaults } else { support.efforts };
            for effort in efforts {
                variants.push(ReasoningVariant {
                    id: effort,
                    effort: Some(effort),
                    budget_tokens: None,
                    disabled: false,
                });
            }
            if support.toggle && !variants.iter().any(|variant| variant.effort == Some("none")) {
                variants.insert(
                    0,
                    ReasoningVariant {
                        id: "none",
                        effort: None,
                        budget_tokens: None,
                        disabled: true,
                    },
                );
            }
        }
        ReasoningProtocol::AnthropicMessages => {
            if support.budget_min.is_some() || support.budget_max.is_some() {
                variants.extend(anthropic_budget_variants(support, output_limit));
            } else if support.toggle
                && let Some(budget) = manual_thinking_budget(output_limit)
            {
                variants.push(ReasoningVariant {
                    id: "none",
                    effort: None,
                    budget_tokens: None,
                    disabled: true,
                });
                variants.push(ReasoningVariant {
                    id: "thinking",
                    effort: None,
                    budget_tokens: Some(budget),
                    disabled: false,
                });
            }
        }
    }
    let mut deduplicated: Vec<ReasoningVariant> = Vec::with_capacity(variants.len());
    for variant in variants {
        if !deduplicated.iter().any(|existing| existing.id == variant.id) {
            deduplicated.push(variant);
        }
    }
    deduplicated
}

/// Finds one variant by exact id.
#[must_use]
pub fn find_variant<'a>(
    variants: &'a [ReasoningVariant],
    id: &str,
) -> Option<&'a ReasoningVariant> {
    variants.iter().find(|variant| variant.id == id)
}

/// Renders the variant ids for a bounded diagnostic.
#[must_use]
pub fn variant_ids(variants: &[ReasoningVariant]) -> String {
    if variants.is_empty() {
        return String::from("none");
    }
    variants.iter().map(|variant| variant.id).collect::<Vec<_>>().join(", ")
}

fn anthropic_budget_variants(
    support: ReasoningSupport,
    output_limit: Option<u64>,
) -> Vec<ReasoningVariant> {
    let ceiling =
        output_limit.unwrap_or(ANTHROPIC_OUTPUT_TOKEN_MAX).min(ANTHROPIC_OUTPUT_TOKEN_MAX);
    let maximum =
        support.budget_max.unwrap_or(ceiling.saturating_sub(1)).min(ceiling.saturating_sub(1));
    if maximum == 0 {
        return Vec::new();
    }
    let high = support
        .budget_min
        .unwrap_or(0)
        .max(maximum.saturating_add(1).saturating_div(2))
        .min(maximum);
    vec![
        ReasoningVariant { id: "high", effort: None, budget_tokens: Some(high), disabled: false },
        ReasoningVariant { id: "max", effort: None, budget_tokens: Some(maximum), disabled: false },
    ]
}

fn manual_thinking_budget(output_limit: Option<u64>) -> Option<u64> {
    let ceiling = output_limit.unwrap_or(ANTHROPIC_OUTPUT_TOKEN_MAX);
    let tokens = ANTHROPIC_MANUAL_THINKING_TOKENS.min(ceiling.saturating_sub(1));
    (tokens >= 1_024).then_some(tokens)
}

/// Total media bytes in one transcript, decoded from base64.
#[must_use]
pub fn media_bytes(transcript: &[ModelMessage]) -> usize {
    transcript.iter().flat_map(|message| message.content.iter()).map(media_block_bytes).sum()
}

/// Largest inline media payload allowed across one request, in decoded bytes.
///
/// The reference harness starts offloading at 25 MiB and brings the request
/// under 15 MiB, so EE uses the same trigger and target.
pub const MEDIA_TRIGGER_BYTES: usize = 25 * 1024 * 1024;
/// Target inline media budget after offload, in decoded bytes.
pub const MEDIA_TARGET_BYTES: usize = 15 * 1024 * 1024;

/// Rewrites a transcript to bring inline media under [`MEDIA_TARGET_BYTES`].
///
/// Returns `None` when nothing needs offloading, so callers keep the borrowed
/// transcript. Oversized blocks are replaced largest-first with a deterministic
/// text placeholder that names only the MIME type and decoded size, so repeated
/// turns produce byte-identical requests and the prompt cache stays stable.
#[must_use]
pub fn bounded_media(transcript: &[ModelMessage]) -> Option<Vec<ModelMessage>> {
    bounded_media_with(transcript, MEDIA_TRIGGER_BYTES, MEDIA_TARGET_BYTES)
}

fn bounded_media_with(
    transcript: &[ModelMessage],
    trigger: usize,
    target: usize,
) -> Option<Vec<ModelMessage>> {
    let total = media_bytes(transcript);
    if total <= trigger {
        return None;
    }
    let mut rewritten = transcript.to_vec();
    let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
    for (message_index, message) in rewritten.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            let bytes = media_block_bytes(block);
            if bytes > 0 {
                blocks.push((bytes, message_index, block_index));
            }
        }
    }
    // Keep in-place ordering deterministic while offloading the largest first.
    blocks.sort_by(|left, right| {
        right.0.cmp(&left.0).then(left.1.cmp(&right.1)).then(left.2.cmp(&right.2))
    });
    let mut remaining = total;
    for (bytes, message_index, block_index) in blocks {
        if remaining <= target {
            break;
        }
        let placeholder =
            offload_placeholder(&rewritten[message_index].content[block_index], bytes, target);
        if let Some(block) = rewritten[message_index].content.get_mut(block_index) {
            *block = ModelContent::Text(placeholder);
            remaining = remaining.saturating_sub(bytes);
        }
    }
    Some(rewritten)
}

/// Replaces media the model does not declare with deterministic text.
///
/// Only content with a known MIME type is checked; an unknown MIME type is not
/// guessed at. An empty declaration means "no metadata", so the pass is skipped
/// for legacy catalog rows. The placeholder keeps the request shape stable and
/// names the MIME type, its decoded size, and the model, so the model knows an
/// attachment was dropped instead of silently losing it.
#[must_use]
pub fn sanitize_modalities(
    modalities: Modalities,
    model_id: &str,
    transcript: &[ModelMessage],
) -> Option<Vec<ModelMessage>> {
    if modalities.input.is_empty() {
        return None;
    }
    let mut rewritten: Option<Vec<ModelMessage>> = None;
    for (message_index, message) in transcript.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            let Some(mime) = media_mime(block) else {
                continue;
            };
            if modalities.accepts(&mime) {
                continue;
            }
            let bytes = media_block_bytes(block);
            let copy = rewritten.get_or_insert_with(|| transcript.to_vec());
            if let Some(slot) = copy[message_index].content.get_mut(block_index) {
                *slot = ModelContent::Text(format!(
                    "[{mime} omitted: {bytes}-byte payload not declared by model {model_id} \
                     (declared: {})]",
                    modalities.describe()
                ));
            }
        }
    }
    rewritten
}

/// One request-level media guard: modality filter plus size offload.
///
/// Returns `None` when the transcript can be used unchanged, and `Some(_)` with
/// a rewritten transcript when an undeclared MIME type was replaced by a
/// placeholder or oversized media had to be offloaded. The modality pass runs
/// before offload so a placeholder never counts against the media budget.
pub fn guard_transcript(
    modalities: Modalities,
    model_id: &str,
    transcript: &[ModelMessage],
) -> Option<Vec<ModelMessage>> {
    match sanitize_modalities(modalities, model_id, transcript) {
        Some(sanitized) => {
            let bounded = bounded_media(&sanitized);
            Some(bounded.unwrap_or(sanitized))
        }
        None => bounded_media(transcript),
    }
}

/// Formats a price in integer micro-dollars as dollars per million tokens.
///
/// Used by `--list-models` so both binaries render catalog prices identically.
#[must_use]
pub fn format_price_micros(micros: u64) -> String {
    let rendered = format!("{:.4}", micros as f64 / 1_000_000.0);
    let trimmed = rendered.trim_end_matches('0').trim_end_matches('.');
    format!("${trimmed}")
}

/// Fills `usage.cost_micros` from catalog prices when the provider did not
/// report a cost.
///
/// Provider-reported cost always wins; an absent price stays absent rather than
/// becoming zero.
pub fn apply_catalog_cost(response: &mut ModelResponse, cost: Option<ModelCost>) {
    if response.usage.cost_micros.is_some() {
        return;
    }
    let Some(cost) = cost else {
        return;
    };
    response.usage.cost_micros = Some(cost.cost_micros(TokenCounts::from_usage(&response.usage)));
}

/// Builds a [`ModelUsage`] with a reported per-turn cost in micro-dollars.
#[must_use]
pub fn usage_with_cost(mut usage: ModelUsage, cost_micros: Option<u64>) -> ModelUsage {
    usage.cost_micros = cost_micros;
    usage
}

fn media_mime(block: &ModelContent) -> Option<String> {
    match block {
        ModelContent::Image { mime_type, .. } | ModelContent::Audio { mime_type, .. } => {
            non_empty_mime(mime_type)
        }
        ModelContent::EmbeddedResource { mime_type, blob: Some(_), .. } => {
            mime_type.as_deref().and_then(non_empty_mime)
        }
        _ => None,
    }
}

fn non_empty_mime(mime: &str) -> Option<String> {
    let mime = mime.trim();
    (!mime.is_empty()).then(|| mime.to_ascii_lowercase())
}

fn media_block_bytes(block: &ModelContent) -> usize {
    match block {
        ModelContent::Image { data, .. } | ModelContent::Audio { data, .. } => base64_bytes(data),
        ModelContent::EmbeddedResource { blob: Some(blob), mime_type, .. } => {
            match mime_type.as_deref() {
                Some(mime)
                    if mime.starts_with("image/")
                        || mime.starts_with("audio/")
                        || mime.starts_with("video/")
                        || mime == "application/pdf" =>
                {
                    base64_bytes(blob)
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

fn base64_bytes(encoded: &str) -> usize {
    let padding = encoded.as_bytes().iter().rev().take_while(|byte| **byte == b'=').count();
    encoded.len().saturating_mul(3).saturating_div(4).saturating_sub(padding)
}

fn offload_placeholder(block: &ModelContent, bytes: usize, target: usize) -> String {
    let mime = media_mime(block).unwrap_or_else(|| String::from("media"));
    format!("[{mime} omitted: {bytes} bytes exceeds the {target}-byte inline media budget]")
}

#[cfg(test)]
mod tests;
