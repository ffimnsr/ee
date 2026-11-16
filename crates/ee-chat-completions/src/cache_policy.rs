//! Prompt-cache policy: where breakpoints go, and which providers honour them.
//!
//! The reference agent applies one protocol-neutral policy before the body
//! builder: the default `auto` shape marks the last tool definition, the first
//! and last system part, and the conversation tail, bounded at four breakpoints
//! per request. Manual hints already placed by the caller are preserved and
//! consume the budget first, so the policy only fills what is left.
//!
//! This module owns the placement plan. Each dialect then lowers the plan into
//! its own marker: `cache_control` parts on Anthropic Messages and
//! OpenRouter/OpenAI-compatible chat, or the request-level lifetime fields on
//! OpenAI Chat/Responses where inline markers are not read.

use serde_json::{Value, json};

/// Anthropic accepts at most four `cache_control` breakpoints per request.
pub const BREAKPOINT_CAP: usize = 4;

/// Cache format a route speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheFormat {
    /// `cache_control: { type: "ephemeral", ttl? }` inline markers.
    #[default]
    Anthropic,
    /// Alibaba Qwen: caches tools together with the system prompt and ignores
    /// breakpoints on tool definitions.
    Alibaba,
}

/// Prompt planes a provider honours breakpoints on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTargets {
    /// Tool definitions.
    pub tools: bool,
    /// System parts.
    pub system: bool,
    /// Conversation messages.
    pub messages: bool,
}

impl CacheTargets {
    /// Every plane.
    pub const ALL: Self = Self { tools: true, system: true, messages: true };
    /// No plane.
    pub const NONE: Self = Self { tools: false, system: false, messages: false };
    /// System and messages only (Qwen caches tools with the system prompt).
    pub const SYSTEM_AND_MESSAGES: Self = Self { tools: false, system: true, messages: true };
}

/// Cache capabilities a route declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSupport {
    /// The provider caches prefixes without any marker.
    pub implicit: bool,
    /// The provider honours inline `cache_control` markers.
    pub explicit: bool,
    /// Breakpoints the provider accepts per request.
    pub max_breakpoints: usize,
    /// Planes the provider honours markers on.
    pub targets: CacheTargets,
    /// Cacheable-prefix floor, when the provider documents one.
    pub min_tokens: Option<u64>,
}

impl CacheSupport {
    /// No caching at all.
    pub const NONE: Self = Self {
        implicit: false,
        explicit: false,
        max_breakpoints: 0,
        targets: CacheTargets::NONE,
        min_tokens: None,
    };

    /// Anthropic-family explicit caching: every plane, four breakpoints, and
    /// the documented 1,024-token minimum.
    pub const ANTHROPIC: Self = Self {
        implicit: false,
        explicit: true,
        max_breakpoints: BREAKPOINT_CAP,
        targets: CacheTargets::ALL,
        min_tokens: Some(1_024),
    };

    /// Provider-managed prefix caching with no inline markers.
    pub const IMPLICIT: Self = Self {
        implicit: true,
        explicit: false,
        max_breakpoints: 0,
        targets: CacheTargets::NONE,
        min_tokens: None,
    };

    /// Whether markers can be emitted at all.
    #[must_use]
    pub const fn accepts_markers(&self) -> bool {
        self.explicit
            && self.max_breakpoints > 0
            && (self.targets.tools || self.targets.system || self.targets.messages)
    }
}

impl Default for CacheSupport {
    fn default() -> Self {
        Self::NONE
    }
}

/// Where the policy places a message-plane breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMessageStrategy {
    /// The last user message.
    LatestUserMessage,
    /// The last assistant message.
    LatestAssistant,
    /// The last `n` eligible messages.
    Tail(usize),
}

/// Auto-placement policy for prompt caching.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CachePolicyObject {
    /// Mark the last tool definition.
    pub tools: bool,
    /// Mark the first and last system part.
    pub system: bool,
    /// Message-plane placement, when any.
    pub messages: Option<CacheMessageStrategy>,
    /// Requested cache lifetime, in the models.dev duration form (`5m`, `1h`,
    /// `24h`). Omitted keeps the provider default.
    pub ttl: Option<String>,
}

impl CachePolicyObject {
    /// The reference `auto` shape.
    #[must_use]
    pub fn auto() -> Self {
        Self { tools: true, system: true, messages: Some(CacheMessageStrategy::Tail(1)), ttl: None }
    }

    /// Qwen's shape: system and tail, never tool definitions.
    #[must_use]
    pub fn qwen() -> Self {
        Self {
            tools: false,
            system: true,
            messages: Some(CacheMessageStrategy::Tail(1)),
            ttl: None,
        }
    }

    /// The same placement with a requested lifetime.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Option<String>) -> Self {
        self.ttl = ttl;
        self
    }
}

/// Cache placement policy for one request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CachePolicy {
    /// Reference default: tools + first/last system + conversation tail.
    #[default]
    Auto,
    /// No automatic placement; manual hints still flow.
    None,
    /// Exactly what the caller asked for.
    Explicit(CachePolicyObject),
}

impl CachePolicy {
    /// Resolves the policy into concrete placement, or `None` when disabled.
    #[must_use]
    pub fn resolve(&self) -> Option<CachePolicyObject> {
        match self {
            Self::Auto => Some(CachePolicyObject::auto()),
            Self::None => None,
            Self::Explicit(object) => Some(object.clone()),
        }
    }

    /// Requested cache lifetime, for routes that send it as a request-level
    /// option instead of on markers.
    #[must_use]
    pub fn ttl(&self) -> Option<&str> {
        match self {
            Self::Explicit(object) => object.ttl.as_deref(),
            Self::Auto | Self::None => None,
        }
    }

    /// Attaches a requested lifetime.
    ///
    /// `Auto` becomes the object form with the same automatic placements, which
    /// is exactly how the reference expresses "auto with a ttl".
    #[must_use]
    pub fn with_ttl(self, ttl: Option<String>) -> Self {
        let Some(ttl) = ttl else {
            return self;
        };
        match self {
            Self::None => Self::None,
            Self::Auto => Self::Explicit(CachePolicyObject::auto().with_ttl(Some(ttl))),
            Self::Explicit(mut object) => {
                object.ttl = Some(ttl);
                Self::Explicit(object)
            }
        }
    }
}

/// Everything one request builder needs to place cache markers.
#[derive(Debug, Clone, Default)]
pub struct CacheDirective {
    /// Placement policy.
    pub policy: CachePolicy,
    /// Route capabilities that gate it.
    pub support: CacheSupport,
    /// Route cache format.
    pub format: CacheFormat,
}

impl CacheDirective {
    /// No caching at all.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Anthropic-family default: auto placement with four breakpoints.
    #[must_use]
    pub fn anthropic() -> Self {
        Self {
            policy: CachePolicy::Auto,
            support: CacheSupport::ANTHROPIC,
            format: CacheFormat::Anthropic,
        }
    }

    /// Provider-managed caching with no inline markers.
    #[must_use]
    pub fn implicit(policy: CachePolicy) -> Self {
        Self { policy, support: CacheSupport::IMPLICIT, format: CacheFormat::Anthropic }
    }

    /// Legacy manual shape: the system prompt plus explicit manual markers, no
    /// automatic tool or tail placement.
    #[must_use]
    pub fn manual_only() -> Self {
        Self {
            policy: CachePolicy::Explicit(CachePolicyObject {
                tools: false,
                system: true,
                messages: None,
                ttl: None,
            }),
            support: CacheSupport::ANTHROPIC,
            format: CacheFormat::Anthropic,
        }
    }

    /// Whether the policy would place inline markers on this route.
    #[must_use]
    pub fn places_markers(&self) -> bool {
        self.support.accepts_markers() && self.policy.resolve().is_some()
    }

    /// Whether any caching is active, implicit or explicit.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.support.implicit || self.places_markers()
    }
}

/// Role of one assembled message, for tail selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheCandidateRole {
    /// System message that is not a separate system part.
    System,
    /// User content.
    User,
    /// Assistant content.
    Assistant,
    /// Tool observation.
    ToolResult,
}

/// One assembled message a policy may mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheCandidate {
    /// Index in the assembled wire message list.
    pub index: usize,
    /// Message role.
    pub role: CacheCandidateRole,
}

/// Inputs the policy plans against.
#[derive(Debug, Clone, Default)]
pub struct CacheCandidates<'a> {
    /// Number of tool definitions in the request.
    pub tools: usize,
    /// Number of system parts (profile prompt plus transcript system messages).
    pub system_parts: usize,
    /// Assembled messages, in wire order.
    pub messages: &'a [CacheCandidate],
    /// Assembled message indexes the caller already marked manually. These are
    /// preserved and consume the budget first.
    pub manual: &'a [usize],
    /// System-part indexes the caller already marked manually.
    pub manual_system: &'a [usize],
    /// Whether a tool observation may carry a breakpoint on this wire.
    pub tool_results: bool,
    /// Measured prompt size, when the caller has one.
    pub prompt_tokens: Option<u64>,
}

/// Where the policy placed breakpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachePlan {
    /// Tool-definition index.
    pub tools: Option<usize>,
    /// System-part indexes.
    pub system: Vec<usize>,
    /// Assembled message indexes (manual first, then automatic).
    pub messages: Vec<usize>,
    /// Placements the caller asked for that did not fit under the cap.
    pub dropped: usize,
}

impl CachePlan {
    /// Total breakpoints the plan places.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.tools.is_some()) + self.system.len() + self.messages.len()
    }

    /// Whether the plan places nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Plans the breakpoints for one request.
///
/// Manual placements are honoured first and never moved; automatic placements
/// fill what is left in reference order (tools, system, messages). A placement
/// that does not fit is counted in [`CachePlan::dropped`] instead of producing
/// an invalid request.
#[must_use]
pub fn plan(directive: &CacheDirective, candidates: &CacheCandidates<'_>) -> CachePlan {
    let mut plan = CachePlan::default();
    if !directive.support.accepts_markers() {
        return plan;
    }
    let Some(policy) = directive.policy.resolve() else {
        return plan;
    };
    let cap = directive.support.max_breakpoints.max(1);

    for &index in candidates.manual {
        if plan.messages.contains(&index) {
            continue;
        }
        if plan.len() >= cap {
            plan.dropped += 1;
            continue;
        }
        plan.messages.push(index);
    }
    for &index in candidates.manual_system {
        if plan.system.contains(&index) {
            continue;
        }
        if plan.len() >= cap {
            plan.dropped += 1;
            continue;
        }
        plan.system.push(index);
    }

    let below_floor = directive
        .support
        .min_tokens
        .zip(candidates.prompt_tokens)
        .is_some_and(|(minimum, measured)| measured < minimum);
    if below_floor {
        return plan;
    }

    if policy.tools
        && directive.support.targets.tools
        && directive.format != CacheFormat::Alibaba
        && candidates.tools > 0
    {
        if plan.len() < cap {
            plan.tools = Some(candidates.tools - 1);
        } else {
            plan.dropped += 1;
        }
    }

    if policy.system && directive.support.targets.system && candidates.system_parts > 0 {
        let last = candidates.system_parts - 1;
        for index in [0, last] {
            if plan.system.contains(&index) {
                continue;
            }
            if plan.len() < cap {
                plan.system.push(index);
            } else {
                plan.dropped += 1;
            }
        }
    }

    if let Some(strategy) = policy.messages
        && directive.support.targets.messages
    {
        for index in select_messages(candidates, strategy) {
            if plan.messages.contains(&index) {
                continue;
            }
            if plan.len() < cap {
                plan.messages.push(index);
            } else {
                plan.dropped += 1;
            }
        }
    }

    plan
}

/// Messages a strategy selects, in wire order.
fn select_messages(candidates: &CacheCandidates<'_>, strategy: CacheMessageStrategy) -> Vec<usize> {
    let eligible = |candidate: &&CacheCandidate| {
        candidate.role != CacheCandidateRole::System
            && (candidates.tool_results || candidate.role != CacheCandidateRole::ToolResult)
    };
    match strategy {
        CacheMessageStrategy::LatestUserMessage => candidates
            .messages
            .iter()
            .filter(eligible)
            .rfind(|candidate| candidate.role == CacheCandidateRole::User)
            .map(|candidate| vec![candidate.index])
            .unwrap_or_default(),
        CacheMessageStrategy::LatestAssistant => candidates
            .messages
            .iter()
            .filter(eligible)
            .rfind(|candidate| candidate.role == CacheCandidateRole::Assistant)
            .map(|candidate| vec![candidate.index])
            .unwrap_or_default(),
        CacheMessageStrategy::Tail(count) => {
            let window: Vec<usize> = candidates
                .messages
                .iter()
                .filter(eligible)
                .rev()
                .take(count)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|candidate| candidate.index)
                .collect();
            window
        }
    }
}

/// `cache_control` marker for one breakpoint, with the configured lifetime.
#[must_use]
pub fn cache_control(directive: &CacheDirective) -> Value {
    match directive.policy.ttl() {
        Some(ttl) => json!({ "type": "ephemeral", "ttl": ttl }),
        None => json!({ "type": "ephemeral" }),
    }
}

/// Request-level cache lifetime fields for OpenAI Chat/Responses.
///
/// `prompt_cache_retention` accepts only `24h`; every other lifetime goes to
/// `prompt_cache_options.ttl`. Returns `None` when no lifetime is configured or
/// the policy places inline markers instead.
#[must_use]
pub fn openai_request_ttl(directive: &CacheDirective) -> Option<Value> {
    if directive.places_markers() {
        return None;
    }
    let ttl = directive.policy.ttl()?;
    Some(if ttl == "24h" {
        json!({ "prompt_cache_retention": ttl })
    } else {
        json!({ "prompt_cache_options": { "ttl": ttl } })
    })
}

/// Emits one bounded warning when the plan could not place every breakpoint.
pub fn warn_dropped(plan: &CachePlan, protocol: &str, cap: usize) {
    if plan.dropped == 0 {
        return;
    }
    tracing::warn!(
        "{protocol}: dropped {} cache breakpoint(s); the API allows at most {cap} per request.",
        plan.dropped
    );
}

/// Gateway policy for an OpenRouter/Vercel-style model id.
///
/// `~anthropic/...` aliases resolve to the same family as `anthropic/...`;
/// Qwen caches system and tail only; everything else keeps no breakpoints
/// because a new cache write per step costs more than it saves.
#[must_use]
pub fn gateway_policy(model_id: &str) -> CachePolicy {
    let id = model_id.trim_start_matches('~').to_ascii_lowercase();
    if id.starts_with("anthropic/") || id.contains("claude") {
        return CachePolicy::Auto;
    }
    if id.starts_with("qwen/") || id.starts_with("alibaba/qwen") {
        return CachePolicy::Explicit(CachePolicyObject::qwen());
    }
    CachePolicy::None
}

/// Parses `auto`, `none`, or the object form of a cache policy.
///
/// # Errors
///
/// Returns guidance naming the rejected value; callers name the variable.
pub fn parse_policy(raw: &str) -> Result<CachePolicy, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(String::from("expected \"auto\", \"none\", or a JSON object"));
    }
    if trimmed.eq_ignore_ascii_case("auto") {
        return Ok(CachePolicy::Auto);
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Ok(CachePolicy::None);
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|error| format!("is not \"auto\", \"none\", or valid JSON: {error}"))?;
    parse_policy_object(&value)
}

fn parse_policy_object(value: &Value) -> Result<CachePolicy, String> {
    let Some(object) = value.as_object() else {
        return Err(String::from("must be a JSON object"));
    };
    let auto = CachePolicyObject::auto();
    let tools = optional_bool(object.get("tools"), auto.tools, "tools")?;
    let system = optional_bool(object.get("system"), auto.system, "system")?;
    let messages = match object.get("messages") {
        None => auto.messages,
        Some(Value::String(strategy)) => match strategy.as_str() {
            "latest-user-message" => Some(CacheMessageStrategy::LatestUserMessage),
            "latest-assistant" => Some(CacheMessageStrategy::LatestAssistant),
            other => {
                return Err(format!(
                    "messages must be \"latest-user-message\", \"latest-assistant\", or {{\"tail\": n}}, got {other:?}"
                ));
            }
        },
        Some(Value::Object(strategy)) => match strategy.get("tail").and_then(Value::as_u64) {
            Some(tail) => {
                Some(CacheMessageStrategy::Tail(usize::try_from(tail).unwrap_or(usize::MAX)))
            }
            None => return Err(String::from("messages.tail must be a positive integer")),
        },
        Some(_) => {
            return Err(String::from(
                "messages must be \"latest-user-message\", \"latest-assistant\", or {\"tail\": n}",
            ));
        }
    };
    let ttl = match object.get("ttl") {
        None => None,
        Some(Value::String(ttl)) => Some(parse_ttl(ttl)?),
        Some(_) => {
            return Err(String::from("ttl must be a duration string such as \"5m\" or \"1h\""));
        }
    };
    Ok(CachePolicy::Explicit(CachePolicyObject { tools, system, messages, ttl }))
}

fn optional_bool(value: Option<&Value>, fallback: bool, field: &str) -> Result<bool, String> {
    match value {
        None => Ok(fallback),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(format!("{field} must be a boolean")),
    }
}

/// Validates a models.dev duration such as `5m`, `1h`, or `24h`.
///
/// # Errors
///
/// Returns guidance when the value is empty or not `<digits><unit>`.
pub fn parse_ttl(raw: &str) -> Result<String, String> {
    let ttl = raw.trim();
    let bytes = ttl.as_bytes();
    let unit = |suffix: &[u8]| matches!(suffix, b"ms" | b"s" | b"m" | b"h" | b"d");
    // Byte slicing keeps multi-byte input from panicking on a char boundary.
    let valid = (bytes.len() > 2
        && bytes[..bytes.len() - 2].iter().all(u8::is_ascii_digit)
        && unit(&bytes[bytes.len() - 2..]))
        || (bytes.len() > 1
            && bytes[..bytes.len() - 1].iter().all(u8::is_ascii_digit)
            && unit(&bytes[bytes.len() - 1..]));
    if valid {
        Ok(ttl.to_string())
    } else {
        Err(format!("cache ttl {ttl:?} must be a duration such as \"5m\", \"1h\", or \"24h\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates<'a>(
        messages: &'a [CacheCandidate],
        manual: &'a [usize],
        tools: usize,
        system_parts: usize,
    ) -> CacheCandidates<'a> {
        CacheCandidates {
            tools,
            system_parts,
            messages,
            manual,
            manual_system: &[],
            tool_results: false,
            prompt_tokens: None,
        }
    }

    fn wire_messages(count: usize) -> Vec<CacheCandidate> {
        (0..count)
            .map(|index| CacheCandidate {
                index,
                role: if index % 2 == 0 {
                    CacheCandidateRole::User
                } else {
                    CacheCandidateRole::Assistant
                },
            })
            .collect()
    }

    #[test]
    fn auto_places_last_tool_both_system_parts_and_the_tail() {
        let messages = wire_messages(10);
        let plan = plan(&CacheDirective::anthropic(), &candidates(&messages, &[], 10, 2));

        assert_eq!(plan.tools, Some(9));
        assert_eq!(plan.system, vec![0, 1]);
        assert_eq!(plan.messages, vec![9]);
        assert_eq!(plan.len(), BREAKPOINT_CAP);
        assert_eq!(plan.dropped, 0);
    }

    #[test]
    fn manual_markers_are_preserved_and_consume_the_budget() {
        let messages = wire_messages(10);
        let plan = plan(&CacheDirective::anthropic(), &candidates(&messages, &[1, 3, 5, 7], 4, 2));

        assert_eq!(plan.messages, vec![1, 3, 5, 7], "manual markers are never moved");
        assert_eq!(plan.tools, None, "no room left for automatic placements");
        assert_eq!(plan.system, Vec::<usize>::new());
        assert_eq!(plan.dropped, 4, "tools, first system, last system, tail");
    }

    #[test]
    fn qwen_policy_marks_system_and_tail_but_never_tools() {
        let directive = CacheDirective {
            policy: CachePolicy::Explicit(CachePolicyObject::qwen()),
            support: CacheSupport {
                targets: CacheTargets::SYSTEM_AND_MESSAGES,
                ..CacheSupport::ANTHROPIC
            },
            format: CacheFormat::Alibaba,
        };
        let messages = wire_messages(4);
        let plan = plan(&directive, &candidates(&messages, &[], 8, 1));

        assert_eq!(plan.tools, None);
        assert_eq!(plan.system, vec![0]);
        assert_eq!(plan.messages, vec![3]);
    }

    #[test]
    fn none_policy_and_markerless_routes_place_nothing() {
        let messages = wire_messages(4);
        let none = CacheDirective { policy: CachePolicy::None, ..CacheDirective::anthropic() };
        assert!(plan(&none, &candidates(&messages, &[1], 8, 2)).is_empty());
        assert!(plan(&CacheDirective::disabled(), &candidates(&messages, &[], 8, 2)).is_empty());
        assert!(
            plan(&CacheDirective::implicit(CachePolicy::Auto), &candidates(&messages, &[], 8, 2))
                .is_empty()
        );
    }

    #[test]
    fn tie_break_is_tools_then_system_then_messages() {
        // Three system parts but only two tool-free slots after the tool mark.
        let directive = CacheDirective {
            policy: CachePolicy::Auto,
            support: CacheSupport { max_breakpoints: 2, ..CacheSupport::ANTHROPIC },
            format: CacheFormat::Anthropic,
        };
        let messages = wire_messages(8);
        let plan = plan(&directive, &candidates(&messages, &[], 4, 3));

        assert_eq!(plan.tools, Some(3));
        assert_eq!(plan.system, vec![0]);
        assert_eq!(plan.messages, Vec::<usize>::new());
        assert_eq!(plan.dropped, 2, "the second system part and the tail did not fit");
    }

    #[test]
    fn tail_skips_tool_results_unless_the_wire_supports_them() {
        let messages = [
            CacheCandidate { index: 0, role: CacheCandidateRole::User },
            CacheCandidate { index: 1, role: CacheCandidateRole::Assistant },
            CacheCandidate { index: 2, role: CacheCandidateRole::ToolResult },
        ];
        let mut inputs = candidates(&messages, &[], 0, 1);
        assert_eq!(plan(&CacheDirective::anthropic(), &inputs).messages, vec![1]);

        inputs.tool_results = true;
        assert_eq!(plan(&CacheDirective::anthropic(), &inputs).messages, vec![2]);
    }

    #[test]
    fn prompt_below_the_documented_floor_places_no_automatic_breakpoints() {
        let messages = wire_messages(3);
        let mut inputs = candidates(&messages, &[2], 2, 1);
        inputs.prompt_tokens = Some(10);
        let plan = plan(&CacheDirective::anthropic(), &inputs);

        assert_eq!(plan.messages, vec![2], "manual markers still flow");
        assert_eq!(plan.tools, None);
    }

    #[test]
    fn gateway_policy_matches_the_reference_families() {
        assert_eq!(gateway_policy("anthropic/claude-sonnet-4"), CachePolicy::Auto);
        assert_eq!(gateway_policy("~anthropic/claude-sonnet-latest"), CachePolicy::Auto);
        assert_eq!(gateway_policy("google/gemini-2.5-pro"), CachePolicy::None);
        assert!(matches!(
            gateway_policy("qwen/qwen3-max"),
            CachePolicy::Explicit(CachePolicyObject { tools: false, system: true, .. })
        ));
    }

    #[test]
    fn request_ttl_lands_in_the_field_each_protocol_documents() {
        let ttl = |value: &str| CacheDirective {
            policy: CachePolicy::Explicit(CachePolicyObject::auto().with_ttl(Some(value.into()))),
            support: CacheSupport::IMPLICIT,
            format: CacheFormat::Anthropic,
        };

        assert_eq!(openai_request_ttl(&ttl("24h")).expect("ttl")["prompt_cache_retention"], "24h");
        assert_eq!(
            openai_request_ttl(&ttl("1h")).expect("ttl")["prompt_cache_options"]["ttl"],
            "1h"
        );
        assert!(openai_request_ttl(&CacheDirective::implicit(CachePolicy::Auto)).is_none());
    }

    #[test]
    fn cache_control_carries_ttl_only_when_configured() {
        let plain = CacheDirective::anthropic();
        assert_eq!(cache_control(&plain), json!({ "type": "ephemeral" }));

        let with_ttl = CacheDirective {
            policy: CachePolicy::Explicit(CachePolicyObject::auto().with_ttl(Some("1h".into()))),
            ..CacheDirective::anthropic()
        };
        assert_eq!(cache_control(&with_ttl), json!({ "type": "ephemeral", "ttl": "1h" }));
    }

    #[test]
    fn policy_parsing_accepts_shorthands_and_the_object_form() {
        assert_eq!(parse_policy("auto").expect("auto"), CachePolicy::Auto);
        assert_eq!(parse_policy("NONE").expect("none"), CachePolicy::None);
        let parsed =
            parse_policy(r#"{"tools":false,"system":true,"messages":{"tail":2},"ttl":"1h"}"#)
                .expect("object");
        assert_eq!(
            parsed,
            CachePolicy::Explicit(CachePolicyObject {
                tools: false,
                system: true,
                messages: Some(CacheMessageStrategy::Tail(2)),
                ttl: Some(String::from("1h")),
            })
        );
        assert!(parse_policy("{").is_err());
        assert!(parse_policy(r#"{"tools":"yes"}"#).is_err());
        assert!(parse_policy(r#"{"ttl":""}"#).is_err());
    }

    #[test]
    fn ttl_validation_accepts_only_durations() {
        assert!(parse_ttl("5m").is_ok());
        assert!(parse_ttl("24h").is_ok());
        assert!(parse_ttl("1500ms").is_ok());
        assert!(parse_ttl("").is_err());
        assert!(parse_ttl("1w").is_err());
        assert!(parse_ttl("h").is_err());
    }
}
