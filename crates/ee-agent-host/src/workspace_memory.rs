//! Host-owned durable workspace-memory facade.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ee_agent_memory::{
    ExportedFact, FactAuthority, FactFreshness, FactKind, FactProvenance, FactQuery, FactState,
    MemoryConfig, MemoryError, MemoryQuotas, MutationApproval, NewWorkspaceFact, RecallResult,
    SelectionReason, WorkspaceExport, WorkspaceFact as MemoryFact, WorkspaceIdentity,
    WorkspaceMemory, WorkspaceRootSet,
};
use ee_mcp::{
    ProxyToolError, WorkspaceFact, WorkspaceFactMutationResult, WorkspaceFactProvenance,
    WorkspaceFactsResult,
};
use sha2::{Digest, Sha256};

use crate::turn_evidence::TurnEvidence;
use crate::workspace_verified_facts::{
    WorkspaceVerifiedFactAuthority, WorkspaceVerifiedFactCandidate, WorkspaceVerifiedFactFreshness,
    WorkspaceVerifiedSourceIdentity, derive_workspace_verified_fact_candidates,
    verified_source_kind,
};

const DEFAULT_NAMESPACE: &str = "default";

pub const DEFAULT_WORKSPACE_MEMORY_EXPIRY_DAYS: u64 = ee_agent_memory::DEFAULT_FACT_EXPIRY_DAYS;
pub const DEFAULT_WORKSPACE_MEMORY_CANDIDATE_RETENTION_DAYS: u64 =
    ee_agent_memory::DEFAULT_CANDIDATE_RETENTION_DAYS;
pub const DEFAULT_WORKSPACE_MEMORY_STALE_RETENTION_DAYS: u64 =
    ee_agent_memory::DEFAULT_STALE_RETENTION_DAYS;
pub const DEFAULT_WORKSPACE_MEMORY_SUPERSEDED_RETENTION_DAYS: u64 =
    ee_agent_memory::DEFAULT_SUPERSEDED_RETENTION_DAYS;

/// Host-owned durable-store quotas. No storage implementation types cross frontend boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMemoryQuotas {
    pub max_value_bytes: usize,
    pub max_active_facts: usize,
    pub max_active_bytes: usize,
    pub max_total_facts: usize,
    pub max_total_bytes: usize,
    pub max_recall_results: usize,
}

impl Default for WorkspaceMemoryQuotas {
    fn default() -> Self {
        let quotas = MemoryQuotas::default();
        Self {
            max_value_bytes: quotas.max_value_bytes,
            max_active_facts: quotas.max_active_facts,
            max_active_bytes: quotas.max_active_bytes,
            max_total_facts: quotas.max_total_facts,
            max_total_bytes: quotas.max_total_bytes,
            max_recall_results: quotas.max_recall_results,
        }
    }
}

impl From<&WorkspaceMemoryQuotas> for MemoryQuotas {
    fn from(value: &WorkspaceMemoryQuotas) -> Self {
        Self {
            max_value_bytes: value.max_value_bytes,
            max_active_facts: value.max_active_facts,
            max_active_bytes: value.max_active_bytes,
            max_total_facts: value.max_total_facts,
            max_total_bytes: value.max_total_bytes,
            max_recall_results: value.max_recall_results,
        }
    }
}

/// Explicit proof that frontend confirmation completed before a management mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMemoryMutationApproval {
    Approved,
}

/// Sanitized host availability state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMemoryAvailability {
    Disabled,
    Unavailable,
    Available,
}

/// Frontend-safe workspace-memory status. Database path and raw errors are omitted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMemoryHostStatus {
    pub enabled: bool,
    pub availability: WorkspaceMemoryAvailability,
    pub trusted_root_count: usize,
    pub primary_workspace_id: Option<String>,
    pub active_facts: usize,
    pub active_bytes: usize,
    pub quotas: WorkspaceMemoryQuotas,
    pub schema_version: u32,
}

/// Stable sanitized management error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMemoryHostErrorCode {
    Disabled,
    Unavailable,
    InvalidFact,
    SensitiveMaterial,
    Conflict,
    NotFound,
    InvalidTransition,
    QuotaExceeded,
    InvalidExport,
}

/// Frontend-safe management failure. Never contains paths, values, or raw database errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMemoryHostError {
    pub code: WorkspaceMemoryHostErrorCode,
    pub message: String,
}

impl std::fmt::Display for WorkspaceMemoryHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WorkspaceMemoryHostError {}

/// Frontend-safe exported provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMemoryExportProvenance {
    pub source_kind: String,
    pub source_id: String,
    pub revision: Option<String>,
    pub fingerprint: Option<String>,
    pub verified_at: Option<String>,
}

/// One bounded exported fact. Value is absent for redacted exports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMemoryExportedFact {
    pub namespace: String,
    pub key: String,
    pub value: Option<String>,
    pub kind: String,
    pub authority: String,
    pub freshness: String,
    pub provenance: WorkspaceMemoryExportProvenance,
    pub expires_at: Option<String>,
    pub content_hash: String,
}

/// Versioned frontend-safe workspace-memory export.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMemoryExportDto {
    pub schema_version: u32,
    pub workspace_id: String,
    pub redacted: bool,
    pub facts: Vec<WorkspaceMemoryExportedFact>,
}

/// Result for clear/import operations without an exact key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMemoryBulkMutationResult {
    pub operation: String,
    pub affected: u64,
}

/// Resolved workspace-memory settings supplied by frontend configuration.
#[derive(Debug, Clone)]
pub struct WorkspaceMemoryHostConfig {
    /// Explicit opt-in. Disabled by default.
    pub enabled: bool,
    /// Explicit trusted workspace roots. Roots must already exist.
    pub trusted_roots: Vec<PathBuf>,
    /// Optional storage override intended for tests and embedding hosts.
    pub database_path: Option<PathBuf>,
    /// Durable-store quotas.
    pub quotas: WorkspaceMemoryQuotas,
    /// Bounded SQLite busy timeout.
    pub busy_timeout: Duration,
    /// Deterministic expiry and historical-record retention policy.
    pub retention: ee_agent_memory::MemoryRetention,
}

impl Default for WorkspaceMemoryHostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_roots: Vec::new(),
            database_path: None,
            quotas: WorkspaceMemoryQuotas::default(),
            busy_timeout: MemoryConfig::default().busy_timeout,
            retention: ee_agent_memory::MemoryRetention::default(),
        }
    }
}

/// Shared manager-owned facade. Never exposes storage paths or raw storage errors.
#[derive(Debug)]
pub(crate) struct WorkspaceMemoryHost {
    service: Option<Arc<WorkspaceMemory>>,
    roots: Option<Arc<WorkspaceRootSet>>,
    primary: Option<WorkspaceIdentity>,
    availability: WorkspaceMemoryAvailability,
    trusted_root_count: usize,
    quotas: WorkspaceMemoryQuotas,
}

impl WorkspaceMemoryHost {
    pub(crate) fn new(config: &WorkspaceMemoryHostConfig) -> Arc<Self> {
        if !config.enabled {
            return Arc::new(Self {
                service: None,
                roots: None,
                primary: None,
                availability: WorkspaceMemoryAvailability::Disabled,
                trusted_root_count: config.trusted_roots.len(),
                quotas: config.quotas.clone(),
            });
        }

        let roots = match WorkspaceRootSet::new(&config.trusted_roots) {
            Ok(roots) if !roots.is_empty() => Arc::new(roots),
            _ => return Arc::new(Self::unavailable(config)),
        };
        // WorkspaceRootSet is sorted and deduplicated by canonical identity.
        let primary = roots.roots().first().cloned();
        let memory_config = MemoryConfig {
            enabled: true,
            quotas: (&config.quotas).into(),
            busy_timeout: config.busy_timeout,
            retention: config.retention.clone(),
        };
        let service = match &config.database_path {
            Some(path) => WorkspaceMemory::at_path(path, memory_config),
            None => WorkspaceMemory::new(memory_config),
        };
        match (service, primary) {
            (Ok(service), Some(primary)) => {
                let now = Utc::now();
                if roots.roots().iter().any(|root| {
                    service.prune_retained(root, now, MutationApproval::Approved).is_err()
                }) {
                    return Arc::new(Self::unavailable(config));
                }
                Arc::new(Self {
                    service: Some(Arc::new(service)),
                    roots: Some(roots),
                    primary: Some(primary),
                    availability: WorkspaceMemoryAvailability::Available,
                    trusted_root_count: config.trusted_roots.len(),
                    quotas: config.quotas.clone(),
                })
            }
            _ => Arc::new(Self::unavailable(config)),
        }
    }

    pub(crate) fn disabled() -> Arc<Self> {
        Self::new(&WorkspaceMemoryHostConfig::default())
    }

    fn unavailable(config: &WorkspaceMemoryHostConfig) -> Self {
        Self {
            service: None,
            roots: None,
            primary: None,
            availability: WorkspaceMemoryAvailability::Unavailable,
            trusted_root_count: config.trusted_roots.len(),
            quotas: config.quotas.clone(),
        }
    }

    fn resolved(
        &self,
    ) -> Result<(&WorkspaceMemory, &WorkspaceRootSet, &WorkspaceIdentity), ProxyToolError> {
        match self.availability {
            WorkspaceMemoryAvailability::Disabled => {
                Err(unavailable("workspace memory is disabled"))
            }
            WorkspaceMemoryAvailability::Unavailable => {
                Err(unavailable("workspace memory is unavailable"))
            }
            WorkspaceMemoryAvailability::Available => {
                match (&self.service, &self.roots, &self.primary) {
                    (Some(service), Some(roots), Some(primary)) => Ok((service, roots, primary)),
                    _ => Err(unavailable("workspace memory is unavailable")),
                }
            }
        }
    }

    pub(crate) fn status(&self) -> WorkspaceMemoryHostStatus {
        let mut status = WorkspaceMemoryHostStatus {
            enabled: self.availability != WorkspaceMemoryAvailability::Disabled,
            availability: self.availability,
            trusted_root_count: self.trusted_root_count,
            primary_workspace_id: self.primary.as_ref().map(|root| root.digest().to_string()),
            active_facts: 0,
            active_bytes: 0,
            quotas: self.quotas.clone(),
            schema_version: ee_agent_memory::SCHEMA_VERSION,
        };
        if let (Some(service), Some(primary)) = (&self.service, &self.primary) {
            match service.status(primary) {
                Ok(memory) => {
                    status.active_facts = memory.active_facts;
                    status.active_bytes = memory.active_bytes;
                }
                Err(_) => status.availability = WorkspaceMemoryAvailability::Unavailable,
            }
        }
        status
    }

    pub(crate) fn list_primary(
        &self,
        limit: usize,
    ) -> Result<WorkspaceFactsResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let result = service
            .list_prefix(primary, DEFAULT_NAMESPACE, limit.min(self.quotas.max_active_facts))
            .map_err(host_memory_error)?;
        Ok(recall_dto(result))
    }

    pub(crate) fn recall_primary(
        &self,
        query: String,
        limit: usize,
    ) -> Result<WorkspaceFactsResult, WorkspaceMemoryHostError> {
        self.recall_primary_with_stale(query, limit, false)
    }

    pub(crate) fn recall_primary_with_stale(
        &self,
        query: String,
        limit: usize,
        include_stale: bool,
    ) -> Result<WorkspaceFactsResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let roots = WorkspaceRootSet::new([primary.canonical_root()]).map_err(host_memory_error)?;
        let result = service
            .recall(
                &roots,
                &FactQuery {
                    text: query,
                    namespace_prefix: Some(DEFAULT_NAMESPACE.to_string()),
                    include_stale,
                    limit: Some(limit.min(self.quotas.max_recall_results)),
                    ..FactQuery::default()
                },
            )
            .map_err(host_memory_error)?;
        Ok(recall_dto(result))
    }

    pub(crate) fn read_primary(
        &self,
        key: String,
    ) -> Result<WorkspaceFact, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        service
            .read(primary, DEFAULT_NAMESPACE, &key)
            .map_err(host_memory_error)?
            .map(|fact| to_dto(fact, Some("exact_key")))
            .ok_or_else(|| host_error(WorkspaceMemoryHostErrorCode::NotFound))
    }

    pub(crate) fn remember_primary_approved(
        &self,
        key: String,
        value: String,
        _approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let source_id = frontend_source_id(primary, &key);
        let fact = new_user_fact(key.clone(), value, "frontend_user_approved", source_id);
        let stored = service
            .remember(primary, fact, MutationApproval::Approved)
            .map_err(host_memory_error)?;
        Ok(WorkspaceFactMutationResult {
            operation: "remembered".to_string(),
            key,
            affected: 1,
            fact: Some(to_dto(stored, None)),
        })
    }

    pub(crate) fn promote_verified_primary_approved(
        &self,
        candidate: WorkspaceVerifiedFactCandidate,
        evidence: &TurnEvidence,
        _approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let rederived = derive_workspace_verified_fact_candidates(evidence)
            .map_err(|_| host_error(WorkspaceMemoryHostErrorCode::InvalidFact))?;
        if !rederived.iter().any(|derived| derived == &candidate)
            || candidate.authority != WorkspaceVerifiedFactAuthority::HostVerified
            || candidate.freshness != WorkspaceVerifiedFactFreshness::RevisionBound
        {
            return Err(host_error(WorkspaceMemoryHostErrorCode::InvalidFact));
        }

        let key = candidate.key.clone();
        let stored = service
            .remember(
                primary,
                NewWorkspaceFact {
                    namespace: DEFAULT_NAMESPACE.to_string(),
                    key: candidate.key,
                    value: candidate.value,
                    kind: FactKind::Validation,
                    authority: FactAuthority::HostVerified,
                    freshness: FactFreshness::RevisionBound,
                    provenance: FactProvenance {
                        source_kind: verified_source_kind().to_string(),
                        source_id: candidate.source_id,
                        source_revision: Some(candidate.source_revision),
                        source_fingerprint: Some(candidate.source_fingerprint),
                        verified_at: None,
                    },
                    expires_at: None,
                    relations: Vec::new(),
                },
                MutationApproval::Approved,
            )
            .map_err(host_memory_error)?;
        Ok(WorkspaceFactMutationResult {
            operation: "verified_promoted".to_string(),
            key,
            affected: 1,
            fact: Some(to_dto(stored, None)),
        })
    }

    pub(crate) fn invalidate_verified_source(
        &self,
        observed: WorkspaceVerifiedSourceIdentity,
    ) -> Result<WorkspaceMemoryBulkMutationResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        if !valid_verified_source_id(&observed.source_id) {
            return Err(host_error(WorkspaceMemoryHostErrorCode::InvalidFact));
        }
        let active = service
            .list_prefix(primary, DEFAULT_NAMESPACE, self.quotas.max_active_facts)
            .map_err(host_memory_error)?;
        let identity_changed = active.hits.iter().any(|hit| {
            hit.fact.authority == FactAuthority::HostVerified
                && hit.fact.freshness == FactFreshness::RevisionBound
                && hit.fact.provenance.source_kind == verified_source_kind()
                && hit.fact.provenance.source_id == observed.source_id
                && (hit.fact.provenance.source_revision.as_deref()
                    != Some(observed.source_revision.as_str())
                    || hit.fact.provenance.source_fingerprint.as_deref()
                        != Some(observed.source_fingerprint.as_str()))
        });
        let affected = if identity_changed {
            // Host lifecycle owns this transition: exact opaque source identity
            // authorizes only active→stale, never creation, replacement, or deletion.
            service
                .mark_stale_by_source(
                    primary,
                    verified_source_kind(),
                    &observed.source_id,
                    MutationApproval::Approved,
                )
                .map_err(host_memory_error)?
        } else {
            0
        };
        Ok(WorkspaceMemoryBulkMutationResult {
            operation: "verified_source_stale".to_string(),
            affected: affected as u64,
        })
    }

    pub(crate) fn forget_primary_approved(
        &self,
        key: String,
        _approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let affected = service
            .forget(primary, DEFAULT_NAMESPACE, &key, MutationApproval::Approved)
            .map_err(host_memory_error)?;
        Ok(WorkspaceFactMutationResult {
            operation: "forgotten".to_string(),
            key,
            affected: affected as u64,
            fact: None,
        })
    }

    pub(crate) fn retract_primary_approved(
        &self,
        key: String,
        _approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        service
            .retract(primary, DEFAULT_NAMESPACE, &key, MutationApproval::Approved)
            .map_err(host_memory_error)?;
        Ok(WorkspaceFactMutationResult {
            operation: "retracted".to_string(),
            key,
            affected: 1,
            fact: None,
        })
    }

    pub(crate) fn clear_primary_approved(
        &self,
        _approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceMemoryBulkMutationResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let affected =
            service.clear(primary, MutationApproval::Approved).map_err(host_memory_error)?;
        Ok(WorkspaceMemoryBulkMutationResult {
            operation: "cleared".to_string(),
            affected: affected as u64,
        })
    }

    pub(crate) fn export_primary_approved(
        &self,
        include_values: bool,
        _approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceMemoryExportDto, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let export = service
            .export(primary, !include_values, MutationApproval::Approved)
            .map_err(host_memory_error)?;
        Ok(export_to_dto(export))
    }

    pub(crate) fn import_primary_approved(
        &self,
        export: WorkspaceMemoryExportDto,
        _approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceMemoryBulkMutationResult, WorkspaceMemoryHostError> {
        let (service, primary) = self.host_resolved()?;
        let affected = service
            .import(primary, export_from_dto(export)?, MutationApproval::Approved)
            .map_err(host_memory_error)?;
        Ok(WorkspaceMemoryBulkMutationResult {
            operation: "imported".to_string(),
            affected: affected as u64,
        })
    }

    fn host_resolved(
        &self,
    ) -> Result<(&WorkspaceMemory, &WorkspaceIdentity), WorkspaceMemoryHostError> {
        match self.availability {
            WorkspaceMemoryAvailability::Disabled => {
                Err(host_error(WorkspaceMemoryHostErrorCode::Disabled))
            }
            WorkspaceMemoryAvailability::Unavailable => {
                Err(host_error(WorkspaceMemoryHostErrorCode::Unavailable))
            }
            WorkspaceMemoryAvailability::Available => match (&self.service, &self.primary) {
                (Some(service), Some(primary)) => Ok((service, primary)),
                _ => Err(host_error(WorkspaceMemoryHostErrorCode::Unavailable)),
            },
        }
    }

    pub(crate) fn remember(
        &self,
        key: String,
        value: String,
        source_id: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        let (service, _, primary) = self.resolved()?;
        let fact = NewWorkspaceFact {
            namespace: DEFAULT_NAMESPACE.to_string(),
            key: key.clone(),
            value,
            kind: FactKind::Convention,
            authority: FactAuthority::UserAsserted,
            freshness: FactFreshness::Current,
            provenance: FactProvenance {
                source_kind: "mcp_user_approved".to_string(),
                source_id,
                source_revision: None,
                source_fingerprint: None,
                verified_at: None,
            },
            expires_at: None,
            relations: Vec::new(),
        };
        let stored =
            service.remember(primary, fact, MutationApproval::Approved).map_err(memory_error)?;
        Ok(WorkspaceFactMutationResult {
            operation: "remembered".to_string(),
            key,
            affected: 1,
            fact: Some(to_dto(stored, None)),
        })
    }

    pub(crate) fn promote_verified(
        &self,
        candidate: WorkspaceVerifiedFactCandidate,
        evidence: &TurnEvidence,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.promote_verified_primary_approved(
            candidate,
            evidence,
            WorkspaceMemoryMutationApproval::Approved,
        )
        .map_err(proxy_host_error)
    }

    pub(crate) fn read(&self, key: String) -> Result<WorkspaceFact, ProxyToolError> {
        let (service, _, primary) = self.resolved()?;
        service
            .read(primary, DEFAULT_NAMESPACE, &key)
            .map_err(memory_error)?
            .map(|fact| to_dto(fact, Some("exact_key")))
            .ok_or_else(|| sanitized("workspace_fact_not_found", "workspace fact not found", false))
    }

    pub(crate) fn recall(&self, query: String) -> Result<WorkspaceFactsResult, ProxyToolError> {
        let (service, roots, _) = self.resolved()?;
        let result = service
            .recall(
                roots,
                &FactQuery {
                    text: query,
                    namespace_prefix: Some(DEFAULT_NAMESPACE.to_string()),
                    ..FactQuery::default()
                },
            )
            .map_err(memory_error)?;
        Ok(recall_dto(result))
    }

    pub(crate) fn forget(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        let (service, _, primary) = self.resolved()?;
        let affected = service
            .forget(primary, DEFAULT_NAMESPACE, &key, MutationApproval::Approved)
            .map_err(memory_error)?;
        Ok(WorkspaceFactMutationResult {
            operation: "forgotten".to_string(),
            key,
            affected: affected as u64,
            fact: None,
        })
    }

    pub(crate) fn list(&self, limit: usize) -> Result<WorkspaceFactsResult, ProxyToolError> {
        self.list_primary(limit).map_err(proxy_host_error)
    }

    pub(crate) fn retract(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.retract_primary_approved(key, WorkspaceMemoryMutationApproval::Approved)
            .map_err(proxy_host_error)
    }

    pub(crate) fn export(&self, include_values: bool) -> Result<serde_json::Value, ProxyToolError> {
        let export = self
            .export_primary_approved(include_values, WorkspaceMemoryMutationApproval::Approved)
            .map_err(proxy_host_error)?;
        serde_json::to_value(export).map_err(|_| {
            sanitized(
                "workspace_memory_export_invalid",
                "workspace-memory export serialization failed",
                false,
            )
        })
    }

    pub(crate) fn decode_import(
        export_json: &str,
    ) -> Result<WorkspaceMemoryExportDto, ProxyToolError> {
        serde_json::from_str(export_json).map_err(|_| {
            sanitized(
                "workspace_memory_invalid_export",
                "workspace-memory import payload is invalid",
                false,
            )
        })
    }

    pub(crate) fn import(
        &self,
        export: WorkspaceMemoryExportDto,
    ) -> Result<serde_json::Value, ProxyToolError> {
        let result = self
            .import_primary_approved(export, WorkspaceMemoryMutationApproval::Approved)
            .map_err(proxy_host_error)?;
        serde_json::to_value(result).map_err(|_| {
            sanitized(
                "workspace_memory_invalid_export",
                "workspace-memory import result serialization failed",
                false,
            )
        })
    }

    pub(crate) fn clear(&self) -> Result<serde_json::Value, ProxyToolError> {
        let result = self
            .clear_primary_approved(WorkspaceMemoryMutationApproval::Approved)
            .map_err(proxy_host_error)?;
        serde_json::to_value(result).map_err(|_| {
            sanitized(
                "workspace_memory_unavailable",
                "workspace-memory clear result serialization failed",
                false,
            )
        })
    }
}

fn proxy_host_error(error: WorkspaceMemoryHostError) -> ProxyToolError {
    let code = match error.code {
        WorkspaceMemoryHostErrorCode::Disabled => "workspace_memory_disabled",
        WorkspaceMemoryHostErrorCode::Unavailable => "workspace_memory_unavailable",
        WorkspaceMemoryHostErrorCode::InvalidFact => "workspace_memory_invalid_fact",
        WorkspaceMemoryHostErrorCode::SensitiveMaterial => "workspace_memory_sensitive_material",
        WorkspaceMemoryHostErrorCode::Conflict => "workspace_memory_conflict",
        WorkspaceMemoryHostErrorCode::NotFound => "workspace_memory_not_found",
        WorkspaceMemoryHostErrorCode::InvalidTransition => "workspace_memory_invalid_transition",
        WorkspaceMemoryHostErrorCode::QuotaExceeded => "workspace_memory_quota_exceeded",
        WorkspaceMemoryHostErrorCode::InvalidExport => "workspace_memory_invalid_export",
    };
    sanitized(code, &error.message, false)
}

fn recall_dto(result: RecallResult) -> WorkspaceFactsResult {
    WorkspaceFactsResult {
        facts: result
            .hits
            .into_iter()
            .map(|hit| to_dto(hit.fact, Some(selection_reason(hit.reason))))
            .collect(),
        total: result.total_matches as u64,
        omitted: result.omitted_count as u64,
        truncated: result.truncated,
    }
}

fn to_dto(fact: MemoryFact, selection_reason: Option<&str>) -> WorkspaceFact {
    WorkspaceFact {
        id: fact.id.0,
        namespace: fact.namespace,
        key: fact.key,
        value: fact.value,
        kind: fact_kind(fact.kind).to_string(),
        authority: authority(fact.authority).to_string(),
        freshness: freshness(fact.freshness).to_string(),
        state: state(fact.state).to_string(),
        provenance: WorkspaceFactProvenance {
            source_kind: fact.provenance.source_kind,
            source_id: fact.provenance.source_id,
            revision: fact.provenance.source_revision,
            fingerprint: fact.provenance.source_fingerprint,
            verified_at: fact.provenance.verified_at.map(|value| value.to_rfc3339()),
        },
        selection_reason: selection_reason.map(str::to_string),
        created_at: fact.created_at.to_rfc3339(),
        updated_at: fact.updated_at.to_rfc3339(),
        expires_at: fact.expires_at.map(|value| value.to_rfc3339()),
        content_hash: fact.content_hash,
        schema_version: fact.schema_version,
    }
}

fn fact_kind(value: FactKind) -> &'static str {
    match value {
        FactKind::Architecture => "architecture",
        FactKind::Constraint => "constraint",
        FactKind::Convention => "convention",
        FactKind::Decision => "decision",
        FactKind::Command => "command",
        FactKind::Validation => "validation",
        FactKind::Ownership => "ownership",
        FactKind::Dependency => "dependency",
        FactKind::EnvironmentRequirement => "environment_requirement",
        FactKind::UserPreference => "user_preference",
    }
}

fn authority(value: FactAuthority) -> &'static str {
    match value {
        FactAuthority::UserAsserted => "user_asserted",
        FactAuthority::HostVerified => "host_verified",
        FactAuthority::AgentCandidate => "agent_candidate",
    }
}

fn freshness(value: FactFreshness) -> &'static str {
    match value {
        FactFreshness::Current => "current",
        FactFreshness::RevisionBound => "revision_bound",
        FactFreshness::Stale => "stale",
    }
}

fn state(value: FactState) -> &'static str {
    match value {
        FactState::Candidate => "candidate",
        FactState::Active => "active",
        FactState::Stale => "stale",
        FactState::Superseded => "superseded",
        FactState::Retracted => "retracted",
    }
}

fn selection_reason(value: SelectionReason) -> &'static str {
    match value {
        SelectionReason::ExactKey => "exact_key",
        SelectionReason::KeyPrefix => "key_prefix",
        SelectionReason::FullText => "full_text",
    }
}

fn new_user_fact(
    key: String,
    value: String,
    source_kind: &str,
    source_id: String,
) -> NewWorkspaceFact {
    NewWorkspaceFact {
        namespace: DEFAULT_NAMESPACE.to_string(),
        key,
        value,
        kind: FactKind::Convention,
        authority: FactAuthority::UserAsserted,
        freshness: FactFreshness::Current,
        provenance: FactProvenance {
            source_kind: source_kind.to_string(),
            source_id,
            source_revision: None,
            source_fingerprint: None,
            verified_at: None,
        },
        expires_at: None,
        relations: Vec::new(),
    }
}

fn valid_verified_source_id(source_id: &str) -> bool {
    source_id.strip_prefix("turn-evidence:sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn frontend_source_id(primary: &WorkspaceIdentity, key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ee.workspace-memory.frontend-source.v1\0");
    digest.update(primary.digest().as_bytes());
    digest.update(b"\0");
    digest.update(key.as_bytes());
    format!("frontend:{:x}", digest.finalize())
}

fn export_to_dto(export: WorkspaceExport) -> WorkspaceMemoryExportDto {
    WorkspaceMemoryExportDto {
        schema_version: export.schema_version,
        workspace_id: export.workspace_digest,
        redacted: export.redacted,
        facts: export
            .facts
            .into_iter()
            .map(|fact| WorkspaceMemoryExportedFact {
                namespace: fact.namespace,
                key: fact.key,
                value: fact.value,
                kind: fact_kind(fact.kind).to_string(),
                authority: authority(fact.authority).to_string(),
                freshness: freshness(fact.freshness).to_string(),
                provenance: WorkspaceMemoryExportProvenance {
                    source_kind: fact.provenance.source_kind,
                    source_id: fact.provenance.source_id,
                    revision: fact.provenance.source_revision,
                    fingerprint: fact.provenance.source_fingerprint,
                    verified_at: fact.provenance.verified_at.map(|value| value.to_rfc3339()),
                },
                expires_at: fact.expires_at.map(|value| value.to_rfc3339()),
                content_hash: fact.content_hash,
            })
            .collect(),
    }
}

fn export_from_dto(
    export: WorkspaceMemoryExportDto,
) -> Result<WorkspaceExport, WorkspaceMemoryHostError> {
    let facts = export
        .facts
        .into_iter()
        .map(|fact| {
            let verified_at = fact
                .provenance
                .verified_at
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|_| host_error(WorkspaceMemoryHostErrorCode::InvalidExport))?;
            let expires_at = fact
                .expires_at
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|_| host_error(WorkspaceMemoryHostErrorCode::InvalidExport))?;
            Ok(ExportedFact {
                namespace: fact.namespace,
                key: fact.key,
                value: fact.value,
                kind: parse_kind(&fact.kind)?,
                authority: parse_authority(&fact.authority)?,
                freshness: parse_freshness(&fact.freshness)?,
                provenance: FactProvenance {
                    source_kind: fact.provenance.source_kind,
                    source_id: fact.provenance.source_id,
                    source_revision: fact.provenance.revision,
                    source_fingerprint: fact.provenance.fingerprint,
                    verified_at,
                },
                expires_at,
                content_hash: fact.content_hash,
            })
        })
        .collect::<Result<Vec<_>, WorkspaceMemoryHostError>>()?;
    Ok(WorkspaceExport {
        schema_version: export.schema_version,
        workspace_digest: export.workspace_id,
        redacted: export.redacted,
        facts,
    })
}

fn parse_kind(value: &str) -> Result<FactKind, WorkspaceMemoryHostError> {
    match value {
        "architecture" => Ok(FactKind::Architecture),
        "constraint" => Ok(FactKind::Constraint),
        "convention" => Ok(FactKind::Convention),
        "decision" => Ok(FactKind::Decision),
        "command" => Ok(FactKind::Command),
        "validation" => Ok(FactKind::Validation),
        "ownership" => Ok(FactKind::Ownership),
        "dependency" => Ok(FactKind::Dependency),
        "environment_requirement" => Ok(FactKind::EnvironmentRequirement),
        "user_preference" => Ok(FactKind::UserPreference),
        _ => Err(host_error(WorkspaceMemoryHostErrorCode::InvalidExport)),
    }
}

fn parse_authority(value: &str) -> Result<FactAuthority, WorkspaceMemoryHostError> {
    match value {
        "user_asserted" => Ok(FactAuthority::UserAsserted),
        "host_verified" => Ok(FactAuthority::HostVerified),
        "agent_candidate" => Ok(FactAuthority::AgentCandidate),
        _ => Err(host_error(WorkspaceMemoryHostErrorCode::InvalidExport)),
    }
}

fn parse_freshness(value: &str) -> Result<FactFreshness, WorkspaceMemoryHostError> {
    match value {
        "current" => Ok(FactFreshness::Current),
        "revision_bound" => Ok(FactFreshness::RevisionBound),
        "stale" => Ok(FactFreshness::Stale),
        _ => Err(host_error(WorkspaceMemoryHostErrorCode::InvalidExport)),
    }
}

fn host_memory_error(error: MemoryError) -> WorkspaceMemoryHostError {
    let code = match error {
        MemoryError::Disabled => WorkspaceMemoryHostErrorCode::Disabled,
        MemoryError::ApprovalRequired | MemoryError::InvalidTransition => {
            WorkspaceMemoryHostErrorCode::InvalidTransition
        }
        MemoryError::InvalidWorkspace(_)
        | MemoryError::Database(_)
        | MemoryError::DatabaseQuarantined { .. }
        | MemoryError::StateDirectoryUnavailable => WorkspaceMemoryHostErrorCode::Unavailable,
        MemoryError::InvalidFact(_) => WorkspaceMemoryHostErrorCode::InvalidFact,
        MemoryError::SensitiveMaterial => WorkspaceMemoryHostErrorCode::SensitiveMaterial,
        MemoryError::Conflict => WorkspaceMemoryHostErrorCode::Conflict,
        MemoryError::NotFound => WorkspaceMemoryHostErrorCode::NotFound,
        MemoryError::QuotaExceeded(_) => WorkspaceMemoryHostErrorCode::QuotaExceeded,
        MemoryError::UnsupportedExport | MemoryError::RedactedImport => {
            WorkspaceMemoryHostErrorCode::InvalidExport
        }
    };
    host_error(code)
}

fn host_error(code: WorkspaceMemoryHostErrorCode) -> WorkspaceMemoryHostError {
    let message = match code {
        WorkspaceMemoryHostErrorCode::Disabled => "workspace memory is disabled",
        WorkspaceMemoryHostErrorCode::Unavailable => "workspace memory is unavailable",
        WorkspaceMemoryHostErrorCode::InvalidFact => "workspace fact is invalid",
        WorkspaceMemoryHostErrorCode::SensitiveMaterial => {
            "workspace fact contains prohibited material"
        }
        WorkspaceMemoryHostErrorCode::Conflict => "workspace fact conflicts with active memory",
        WorkspaceMemoryHostErrorCode::NotFound => "workspace fact not found",
        WorkspaceMemoryHostErrorCode::InvalidTransition => "workspace-memory transition is invalid",
        WorkspaceMemoryHostErrorCode::QuotaExceeded => "workspace-memory quota exceeded",
        WorkspaceMemoryHostErrorCode::InvalidExport => "workspace-memory export is invalid",
    };
    WorkspaceMemoryHostError { code, message: message.to_string() }
}

fn memory_error(error: MemoryError) -> ProxyToolError {
    match error {
        MemoryError::Disabled => unavailable("workspace memory is disabled"),
        MemoryError::ApprovalRequired => sanitized(
            "workspace_memory_approval_required",
            "workspace-memory mutation requires approval",
            true,
        ),
        MemoryError::InvalidWorkspace(_) => unavailable("workspace memory is unavailable"),
        MemoryError::InvalidFact(_) => {
            sanitized("workspace_fact_invalid", "workspace fact is invalid", false)
        }
        MemoryError::SensitiveMaterial => sanitized(
            "workspace_fact_rejected",
            "workspace fact contains prohibited material",
            false,
        ),
        MemoryError::Conflict => sanitized(
            "workspace_fact_conflict",
            "active workspace fact conflicts with requested value",
            false,
        ),
        MemoryError::NotFound => {
            sanitized("workspace_fact_not_found", "workspace fact not found", false)
        }
        MemoryError::InvalidTransition => sanitized(
            "workspace_fact_transition_invalid",
            "workspace fact transition is invalid",
            false,
        ),
        MemoryError::QuotaExceeded(_) => {
            sanitized("workspace_memory_quota_exceeded", "workspace-memory quota exceeded", false)
        }
        MemoryError::UnsupportedExport | MemoryError::RedactedImport => sanitized(
            "workspace_memory_operation_invalid",
            "workspace-memory operation is invalid",
            false,
        ),
        MemoryError::Database(_)
        | MemoryError::DatabaseQuarantined { .. }
        | MemoryError::StateDirectoryUnavailable => unavailable("workspace memory is unavailable"),
    }
}

fn unavailable(message: &str) -> ProxyToolError {
    sanitized("workspace_memory_unavailable", message, false)
}

fn sanitized(code: &str, message: &str, is_permission_denied: bool) -> ProxyToolError {
    ProxyToolError { message: format!("{code}: {message}"), is_permission_denied }
}

#[cfg(test)]
#[path = "workspace_memory_tests.rs"]
mod tests;
