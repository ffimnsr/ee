use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_MAX_VALUE_BYTES: usize = 4 * 1024;
pub const DEFAULT_MAX_ACTIVE_FACTS: usize = 256;
pub const DEFAULT_MAX_ACTIVE_BYTES: usize = 512 * 1024;
pub const DEFAULT_MAX_TOTAL_FACTS: usize = 256;
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 512 * 1024;
pub const DEFAULT_MAX_RECALL_RESULTS: usize = 8;
pub const DEFAULT_FACT_EXPIRY_DAYS: u64 = 0;
pub const DEFAULT_CANDIDATE_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_STALE_RETENTION_DAYS: u64 = 30;
pub const DEFAULT_SUPERSEDED_RETENTION_DAYS: u64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FactId(pub i64);

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl $name {
            pub(crate) const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
            pub(crate) fn parse(value: &str) -> Result<Self, MemoryError> {
                match value { $($value => Ok(Self::$variant),)+ _ => Err(MemoryError::Database("invalid stored enum")) }
            }
        }
    };
}

string_enum!(FactKind {
    Architecture => "architecture", Constraint => "constraint", Convention => "convention",
    Decision => "decision", Command => "command", Validation => "validation",
    Ownership => "ownership", Dependency => "dependency",
    EnvironmentRequirement => "environment_requirement", UserPreference => "user_preference"
});
string_enum!(FactAuthority {
    UserAsserted => "user_asserted", HostVerified => "host_verified", AgentCandidate => "agent_candidate"
});
string_enum!(FactFreshness { Current => "current", RevisionBound => "revision_bound", Stale => "stale" });
string_enum!(FactState {
    Candidate => "candidate", Active => "active", Stale => "stale",
    Superseded => "superseded", Retracted => "retracted"
});
string_enum!(FactRelationKind {
    Supersedes => "supersedes", AppliesTo => "applies_to", DependsOn => "depends_on", Contradicts => "contradicts"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactProvenance {
    pub source_kind: String,
    pub source_id: String,
    pub source_revision: Option<String>,
    pub source_fingerprint: Option<String>,
    pub verified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactRelation {
    pub kind: FactRelationKind,
    pub target: FactId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFact {
    pub id: FactId,
    pub workspace_digest: String,
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub kind: FactKind,
    pub authority: FactAuthority,
    pub freshness: FactFreshness,
    pub state: FactState,
    pub provenance: FactProvenance,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub content_hash: String,
    pub schema_version: u32,
    pub supersedes: Option<FactId>,
    pub relations: Vec<FactRelation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkspaceFact {
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub kind: FactKind,
    pub authority: FactAuthority,
    pub freshness: FactFreshness,
    pub provenance: FactProvenance,
    pub expires_at: Option<DateTime<Utc>>,
    pub relations: Vec<FactRelation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactQuery {
    pub text: String,
    pub namespace_prefix: Option<String>,
    pub kinds: BTreeSet<FactKind>,
    pub authorities: BTreeSet<FactAuthority>,
    /// Include explicitly stale facts. Superseded and retracted history remains excluded.
    pub include_stale: bool,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    ExactKey,
    KeyPrefix,
    FullText,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallHit {
    pub fact: WorkspaceFact,
    pub reason: SelectionReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallResult {
    pub hits: Vec<RecallHit>,
    pub total_matches: usize,
    pub omitted_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationApproval {
    Approved,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryQuotas {
    pub max_value_bytes: usize,
    pub max_active_facts: usize,
    pub max_active_bytes: usize,
    pub max_total_facts: usize,
    pub max_total_bytes: usize,
    pub max_recall_results: usize,
}
impl Default for MemoryQuotas {
    fn default() -> Self {
        Self {
            max_value_bytes: DEFAULT_MAX_VALUE_BYTES,
            max_active_facts: DEFAULT_MAX_ACTIVE_FACTS,
            max_active_bytes: DEFAULT_MAX_ACTIVE_BYTES,
            max_total_facts: DEFAULT_MAX_TOTAL_FACTS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_recall_results: DEFAULT_MAX_RECALL_RESULTS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRetention {
    /// Default lifetime for facts without explicit expiry. `None` preserves them indefinitely.
    pub default_expiry: Option<Duration>,
    /// Maximum age of unverified agent candidates.
    pub candidate_retention: Duration,
    /// Maximum age of stale and retracted facts.
    pub stale_retention: Duration,
    /// Maximum age of superseded fact history.
    pub superseded_retention: Duration,
}

impl Default for MemoryRetention {
    fn default() -> Self {
        Self {
            default_expiry: None,
            candidate_retention: Duration::from_secs(DEFAULT_CANDIDATE_RETENTION_DAYS * 86_400),
            stale_retention: Duration::from_secs(DEFAULT_STALE_RETENTION_DAYS * 86_400),
            superseded_retention: Duration::from_secs(DEFAULT_SUPERSEDED_RETENTION_DAYS * 86_400),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub quotas: MemoryQuotas,
    pub busy_timeout: Duration,
    pub retention: MemoryRetention,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quotas: MemoryQuotas::default(),
            busy_timeout: Duration::from_secs(2),
            retention: MemoryRetention::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMemoryStatus {
    pub enabled: bool,
    pub database_path: PathBuf,
    pub active_facts: usize,
    pub active_bytes: usize,
    pub quotas: MemoryQuotas,
    pub schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceExport {
    pub schema_version: u32,
    pub workspace_digest: String,
    pub redacted: bool,
    pub facts: Vec<ExportedFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedFact {
    pub namespace: String,
    pub key: String,
    pub value: Option<String>,
    pub kind: FactKind,
    pub authority: FactAuthority,
    pub freshness: FactFreshness,
    pub provenance: FactProvenance,
    pub expires_at: Option<DateTime<Utc>>,
    pub content_hash: String,
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("workspace memory is disabled")]
    Disabled,
    #[error("persistent mutation requires explicit approval")]
    ApprovalRequired,
    #[error("invalid workspace: {0}")]
    InvalidWorkspace(&'static str),
    #[error("invalid fact: {0}")]
    InvalidFact(&'static str),
    #[error("sensitive or session-history material rejected")]
    SensitiveMaterial,
    #[error("fact conflict for active key")]
    Conflict,
    #[error("fact not found")]
    NotFound,
    #[error("invalid authority or lifecycle transition")]
    InvalidTransition,
    #[error("quota exceeded: {0}")]
    QuotaExceeded(&'static str),
    #[error("database unavailable: {0}")]
    Database(&'static str),
    #[error("database quarantined at {path}")]
    DatabaseQuarantined { path: PathBuf },
    #[error("platform state directory unavailable")]
    StateDirectoryUnavailable,
    #[error("unsupported export schema")]
    UnsupportedExport,
    #[error("redacted exports cannot be imported")]
    RedactedImport,
}
