//! Declared vendor families for catalog routes.
//!
//! Rubber-duck contrast uses declared identity only: the orchestrator registry
//! refuses to pair a critic with a model that shares its family, so a critic is
//! meaningful only when both models have a real, different family. EE never
//! guesses a vendor from a model-name prefix, so the family is explicit data
//! here, next to — but separate from — the routing table.
//!
//! Routing never reads this module. A stale or missing family cannot change an
//! endpoint, a dialect, or where a credential goes; it can only make contrast
//! unavailable, and the completeness tests fail when a catalog row has no
//! declared family or when a family entry names a model that is not routable.

use ee_agent_orchestrator::ModelFamily;

use crate::routes::OpenCodeRoute;

/// Vendor identity as declared in this table.
///
/// Kept separate from [`ModelFamily`] so the table stays const-friendly; the
/// conversion is total and validated by the orchestrator when identities are
/// built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclaredVendor {
    /// OpenAI-published ids.
    OpenAi,
    /// Anthropic-published ids.
    Anthropic,
    /// xAI-published ids.
    Xai,
    /// Alibaba Qwen ids.
    Qwen,
    /// DeepSeek ids.
    DeepSeek,
    /// Any other documented vendor, by its documented slug.
    Named(&'static str),
}

impl DeclaredVendor {
    fn to_family(self) -> ModelFamily {
        match self {
            Self::OpenAi => ModelFamily::OpenAi,
            Self::Anthropic => ModelFamily::Anthropic,
            Self::Xai => ModelFamily::Xai,
            Self::Qwen => ModelFamily::Qwen,
            Self::DeepSeek => ModelFamily::DeepSeek,
            Self::Named(slug) => ModelFamily::Other(slug.to_string()),
        }
    }

    /// Stable label used in diagnostics and tests.
    #[must_use]
    fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Xai => "xai",
            Self::Qwen => "qwen",
            Self::DeepSeek => "deepseek",
            Self::Named(slug) => slug,
        }
    }
}

/// One vendor and the model ids it publishes in the OpenCode catalog.
struct VendorGroup {
    vendor: DeclaredVendor,
    model_ids: &'static [&'static str],
}

/// Declared vendor groups, keyed by model id.
///
/// A model id is the same vendor on both surfaces, so the table lists each id
/// once. Ids are copied from upstream vendor naming, never derived by prefix
/// matching at runtime.
const VENDORS: &[VendorGroup] = &[
    VendorGroup {
        vendor: DeclaredVendor::OpenAi,
        model_ids: &[
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-5.4",
            "gpt-5.4-pro",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.3-codex",
            "gpt-5.3-codex-spark",
            "gpt-5.2",
            "gpt-5.1",
            "gpt-5",
            "gpt-5-nano",
        ],
    },
    VendorGroup {
        vendor: DeclaredVendor::Xai,
        model_ids: &["grok-4.7", "grok-4.6", "grok-4.5", "grok-build-0.1"],
    },
    VendorGroup {
        vendor: DeclaredVendor::Anthropic,
        model_ids: &[
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ],
    },
    VendorGroup {
        vendor: DeclaredVendor::Qwen,
        model_ids: &[
            "qwen3.8-max",
            "qwen3.8-flash",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.6-plus",
            "qwen3.5-plus",
        ],
    },
    VendorGroup {
        vendor: DeclaredVendor::DeepSeek,
        model_ids: &[
            "deepseek-v4.1-flash",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
        ],
    },
    VendorGroup {
        vendor: DeclaredVendor::Named("minimax"),
        model_ids: &["minimax-m3", "minimax-m2.7", "minimax-m2.5"],
    },
    VendorGroup {
        vendor: DeclaredVendor::Named("zhipu"),
        model_ids: &["glm-5.3-flash", "glm-5.3", "glm-5.2", "glm-5.1"],
    },
    VendorGroup {
        vendor: DeclaredVendor::Named("moonshot"),
        model_ids: &["kimi-k3", "kimi-k2.7-code", "kimi-k2.6"],
    },
    VendorGroup {
        vendor: DeclaredVendor::Named("mimo"),
        model_ids: &[
            "mimo-v2.6-flash",
            "mimo-v2.6-flash-free",
            "mimo-v2.6-pro",
            "mimo-v2.5",
            "mimo-v2.5-pro",
            "mimo-v2.5-free",
        ],
    },
    VendorGroup {
        vendor: DeclaredVendor::Named("muse"),
        model_ids: &[
            "muse-spark-1.3",
            "muse-spark-1.2",
            "muse-spark-1.3-contributor",
            "muse-spark-1.2-contributor",
            "muse-spark-1.3-contributor-free",
        ],
    },
    VendorGroup { vendor: DeclaredVendor::Named("longcat"), model_ids: &["longcat-2.0"] },
    VendorGroup { vendor: DeclaredVendor::Named("hy"), model_ids: &["hy4-preview", "hy3"] },
    VendorGroup { vendor: DeclaredVendor::Named("big-pickle"), model_ids: &["big-pickle"] },
    VendorGroup { vendor: DeclaredVendor::Named("space-bunny"), model_ids: &["space-bunny-free"] },
    VendorGroup { vendor: DeclaredVendor::Named("ling"), model_ids: &["ling-3.0-flash-fin-free"] },
    VendorGroup {
        vendor: DeclaredVendor::Named("nvidia"),
        model_ids: &["nemotron-3-ultra-free", "nemotron-3.5-lightning-free"],
    },
];

/// Declared family for one catalog model id.
///
/// Returns `None` for an id the table does not declare, which is a table defect
/// rather than a routing result; callers must treat it as "contrast
/// unavailable" and never as permission to guess a vendor.
#[must_use]
pub fn family_for(model_id: &str) -> Option<ModelFamily> {
    VENDORS
        .iter()
        .find(|group| group.model_ids.contains(&model_id))
        .map(|group| group.vendor.to_family())
}

/// Declared family for one resolved route.
#[must_use]
pub fn route_family(route: &OpenCodeRoute) -> Option<ModelFamily> {
    family_for(route.model_id)
}

/// Bounded label for diagnostics: the declared family of a route, or `unknown`.
#[must_use]
pub fn family_label(route: &OpenCodeRoute) -> String {
    VENDORS
        .iter()
        .find(|group| group.model_ids.contains(&route.model_id))
        .map_or_else(|| String::from("unknown"), |group| group.vendor.label().to_string())
}

/// Every declared model id, in table order.
#[must_use]
pub fn declared_ids() -> Vec<&'static str> {
    VENDORS.iter().flat_map(|group| group.model_ids.iter().copied()).collect()
}

#[cfg(test)]
mod tests;
