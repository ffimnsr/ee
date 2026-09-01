use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    ExportedFact, FactAuthority, FactFreshness, FactId, FactKind, FactProvenance, FactQuery,
    FactRelation, FactRelationKind, FactState, MemoryConfig, MemoryError, MutationApproval,
    NewWorkspaceFact, RecallHit, RecallResult, SCHEMA_VERSION, SelectionReason, WorkspaceExport,
    WorkspaceFact, WorkspaceIdentity, WorkspaceMemoryStatus, WorkspaceRootSet,
    migrations::{self, db},
    validation::{content_hash, initial_state, normalize_component, validate_fact},
};

#[derive(Debug, Clone)]
pub struct WorkspaceMemory {
    path: PathBuf,
    config: MemoryConfig,
}

impl WorkspaceMemory {
    pub fn new(config: MemoryConfig) -> Result<Self, MemoryError> {
        let state = dirs::state_dir().ok_or(MemoryError::StateDirectoryUnavailable)?;
        Self::at_path(state.join("ee").join("workspace-memory.sqlite3"), config)
    }

    /// Test/embedding constructor with explicit database path.
    pub fn at_path(path: impl Into<PathBuf>, config: MemoryConfig) -> Result<Self, MemoryError> {
        let this = Self { path: path.into(), config };
        if this.config.enabled {
            this.initialize()?;
        }
        Ok(this)
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.path
    }

    pub fn status(
        &self,
        workspace: &WorkspaceIdentity,
    ) -> Result<WorkspaceMemoryStatus, MemoryError> {
        if !self.config.enabled {
            return Ok(WorkspaceMemoryStatus {
                enabled: false,
                database_path: self.path.clone(),
                active_facts: 0,
                active_bytes: 0,
                quotas: self.config.quotas.clone(),
                schema_version: SCHEMA_VERSION,
            });
        }
        let connection = self.connection()?;
        let (active_facts, active_bytes) = usage(&connection, workspace.digest())?;
        Ok(WorkspaceMemoryStatus {
            enabled: true,
            database_path: self.path.clone(),
            active_facts,
            active_bytes,
            quotas: self.config.quotas.clone(),
            schema_version: SCHEMA_VERSION,
        })
    }

    pub fn remember(
        &self,
        workspace: &WorkspaceIdentity,
        fact: NewWorkspaceFact,
        approval: MutationApproval,
    ) -> Result<WorkspaceFact, MemoryError> {
        self.mutate(approval)?;
        let (namespace, key) = validate_fact(&fact, &self.config.quotas)?;
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        ensure_workspace(&tx, workspace)?;
        if let Some(existing) = active_by_key(&tx, workspace.digest(), &namespace, &key)? {
            if existing.value == fact.value && existing.authority == fact.authority {
                tx.commit().map_err(db)?;
                return Ok(existing);
            }
            return Err(MemoryError::Conflict);
        }
        enforce_quota(
            &tx,
            workspace.digest(),
            fact.value.len(),
            1,
            fact.value.len(),
            1,
            &self.config,
        )?;
        let inserted =
            insert_fact(&tx, workspace.digest(), &namespace, &key, &fact, None, &self.config)?;
        tx.commit().map_err(db)?;
        Ok(inserted)
    }

    pub fn replace(
        &self,
        workspace: &WorkspaceIdentity,
        fact: NewWorkspaceFact,
        approval: MutationApproval,
    ) -> Result<WorkspaceFact, MemoryError> {
        self.mutate(approval)?;
        let (namespace, key) = validate_fact(&fact, &self.config.quotas)?;
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        ensure_workspace(&tx, workspace)?;
        let old = active_by_key(&tx, workspace.digest(), &namespace, &key)?
            .ok_or(MemoryError::NotFound)?;
        let added_bytes = fact.value.len().saturating_sub(old.value.len());
        enforce_quota(&tx, workspace.digest(), added_bytes, 0, fact.value.len(), 1, &self.config)?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE facts SET state='superseded', updated_at=?1 WHERE id=?2 AND state='active'",
            params![now, old.id.0],
        )
        .map_err(db)?;
        let inserted = insert_fact(
            &tx,
            workspace.digest(),
            &namespace,
            &key,
            &fact,
            Some(old.id),
            &self.config,
        )?;
        tx.commit().map_err(db)?;
        Ok(inserted)
    }

    /// Verifies and promotes an agent candidate to durable active authority.
    pub fn verify_candidate(
        &self,
        workspace: &WorkspaceIdentity,
        id: FactId,
        authority: FactAuthority,
        approval: MutationApproval,
    ) -> Result<WorkspaceFact, MemoryError> {
        self.promote_candidate(workspace, id, authority, approval)
    }

    /// Promotes an agent candidate after user or host verification.
    pub fn promote_candidate(
        &self,
        workspace: &WorkspaceIdentity,
        id: FactId,
        authority: FactAuthority,
        approval: MutationApproval,
    ) -> Result<WorkspaceFact, MemoryError> {
        self.mutate(approval)?;
        if authority == FactAuthority::AgentCandidate {
            return Err(MemoryError::InvalidTransition);
        }
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let candidate = fact_by_id(&tx, workspace.digest(), id)?.ok_or(MemoryError::NotFound)?;
        if candidate.state != FactState::Candidate
            || candidate.authority != FactAuthority::AgentCandidate
        {
            return Err(MemoryError::InvalidTransition);
        }
        if active_by_key(&tx, workspace.digest(), &candidate.namespace, &candidate.key)?.is_some() {
            return Err(MemoryError::Conflict);
        }
        enforce_quota(&tx, workspace.digest(), candidate.value.len(), 1, 0, 0, &self.config)?;
        let now = Utc::now().to_rfc3339();
        tx.execute("UPDATE facts SET authority=?1, state='active', freshness='current', verified_at=?2, updated_at=?2 WHERE id=?3",
            params![authority.as_str(), now, id.0]).map_err(db)?;
        tx.commit().map_err(db)?;
        self.read_by_id(workspace, id)?.ok_or(MemoryError::NotFound)
    }

    pub fn read(
        &self,
        workspace: &WorkspaceIdentity,
        namespace: &str,
        key: &str,
    ) -> Result<Option<WorkspaceFact>, MemoryError> {
        self.enabled()?;
        let namespace = normalize_component(namespace)?;
        let key = normalize_component(key)?;
        let connection = self.connection()?;
        let fact = active_by_key(&connection, workspace.digest(), &namespace, &key)?;
        Ok(fact.filter(current_and_unexpired))
    }

    pub fn list_prefix(
        &self,
        workspace: &WorkspaceIdentity,
        namespace_prefix: &str,
        limit: usize,
    ) -> Result<RecallResult, MemoryError> {
        self.enabled()?;
        let prefix = normalize_component(namespace_prefix)?;
        let connection = self.connection()?;
        let pattern = format!("{}%", escape_like(&prefix));
        let mut statement = connection.prepare(
            "SELECT f.* FROM facts f WHERE f.workspace_digest=?1 AND f.namespace LIKE ?2 ESCAPE '\\'
             AND f.state='active' AND f.freshness!='stale' AND (f.expires_at IS NULL OR f.expires_at>?3)
             ORDER BY f.namespace, f.normalized_key, f.id"
        ).map_err(db)?;
        let facts = collect_facts(
            &connection,
            &mut statement,
            params![workspace.digest(), pattern, Utc::now().to_rfc3339()],
        )?;
        Ok(bounded(
            facts
                .into_iter()
                .map(|fact| RecallHit { fact, reason: SelectionReason::KeyPrefix })
                .collect(),
            limit,
        ))
    }

    pub fn recall(
        &self,
        roots: &WorkspaceRootSet,
        query: &FactQuery,
    ) -> Result<RecallResult, MemoryError> {
        self.enabled()?;
        if roots.is_empty() {
            return Ok(bounded(vec![], 0));
        }
        let limit = query
            .limit
            .unwrap_or(self.config.quotas.max_recall_results)
            .min(self.config.quotas.max_recall_results);
        let text = query.text.trim().to_ascii_lowercase();
        if text.is_empty() {
            return Ok(bounded(vec![], limit));
        }
        let connection = self.connection()?;
        let mut hits = Vec::new();
        let mut seen = HashSet::new();
        for reason in
            [SelectionReason::ExactKey, SelectionReason::KeyPrefix, SelectionReason::FullText]
        {
            for root in roots.roots() {
                for fact in
                    recall_stage(&connection, root.digest(), &text, reason, query.include_stale)?
                {
                    if seen.insert(fact.id.0) && matches_query(&fact, query) {
                        hits.push(RecallHit { fact, reason });
                    }
                }
            }
        }
        Ok(bounded(hits, limit))
    }

    pub fn retract(
        &self,
        workspace: &WorkspaceIdentity,
        namespace: &str,
        key: &str,
        approval: MutationApproval,
    ) -> Result<(), MemoryError> {
        self.transition_key(workspace, namespace, key, FactState::Retracted, approval)
    }

    pub fn mark_stale_by_source(
        &self,
        workspace: &WorkspaceIdentity,
        source_kind: &str,
        source_id: &str,
        approval: MutationApproval,
    ) -> Result<usize, MemoryError> {
        self.mutate(approval)?;
        if source_kind.is_empty() || source_id.is_empty() {
            return Err(MemoryError::InvalidFact("source identity required"));
        }
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let changed = tx
            .execute(
                "UPDATE facts SET state='stale', freshness='stale', updated_at=?1
             WHERE workspace_digest=?2 AND source_kind=?3 AND source_id=?4 AND state='active'",
                params![Utc::now().to_rfc3339(), workspace.digest(), source_kind, source_id],
            )
            .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(changed)
    }

    pub fn forget(
        &self,
        workspace: &WorkspaceIdentity,
        namespace: &str,
        key: &str,
        approval: MutationApproval,
    ) -> Result<usize, MemoryError> {
        self.mutate(approval)?;
        let namespace = normalize_component(namespace)?;
        let key = normalize_component(key)?;
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let ids = ids_by_key(&tx, workspace.digest(), &namespace, &key)?;
        for id in &ids {
            tx.execute("DELETE FROM facts_fts WHERE fact_id=?1", [id]).map_err(db)?;
        }
        tx.execute(
            "DELETE FROM facts WHERE workspace_digest=?1 AND namespace=?2 AND normalized_key=?3",
            params![workspace.digest(), namespace, key],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(ids.len())
    }

    pub fn clear(
        &self,
        workspace: &WorkspaceIdentity,
        approval: MutationApproval,
    ) -> Result<usize, MemoryError> {
        self.mutate(approval)?;
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM facts WHERE workspace_digest=?1",
                [workspace.digest()],
                |row| row.get(0),
            )
            .map_err(db)?;
        tx.execute("DELETE FROM facts_fts WHERE workspace_digest=?1", [workspace.digest()])
            .map_err(db)?;
        tx.execute("DELETE FROM facts WHERE workspace_digest=?1", [workspace.digest()])
            .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(count as usize)
    }

    pub fn prune(
        &self,
        workspace: &WorkspaceIdentity,
        history_before: DateTime<Utc>,
        approval: MutationApproval,
    ) -> Result<usize, MemoryError> {
        self.mutate(approval)?;
        self.prune_where(
            "((state IN ('stale','superseded','retracted','candidate') AND updated_at<=?2)\
             OR (expires_at IS NOT NULL AND expires_at<=?3))",
            params![workspace.digest(), history_before.to_rfc3339(), Utc::now().to_rfc3339()],
        )
    }

    /// Applies configured retention using caller-supplied time for deterministic lifecycle runs.
    pub fn prune_retained(
        &self,
        workspace: &WorkspaceIdentity,
        now: DateTime<Utc>,
        approval: MutationApproval,
    ) -> Result<usize, MemoryError> {
        self.mutate(approval)?;
        let candidate_before = retention_cutoff(now, self.config.retention.candidate_retention)?;
        let stale_before = retention_cutoff(now, self.config.retention.stale_retention)?;
        let superseded_before = retention_cutoff(now, self.config.retention.superseded_retention)?;
        self.prune_where(
            "((state='candidate' AND updated_at<=?2)\
             OR (state IN ('stale','retracted') AND updated_at<=?3)\
             OR (state='superseded' AND updated_at<=?4)\
             OR (expires_at IS NOT NULL AND expires_at<=?5))",
            params![
                workspace.digest(),
                candidate_before.to_rfc3339(),
                stale_before.to_rfc3339(),
                superseded_before.to_rfc3339(),
                now.to_rfc3339()
            ],
        )
    }

    fn prune_where<P: rusqlite::Params>(
        &self,
        predicate: &str,
        parameters: P,
    ) -> Result<usize, MemoryError> {
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let sql =
            format!("SELECT id FROM facts WHERE workspace_digest=?1 AND {predicate} ORDER BY id");
        let mut statement = tx.prepare(&sql).map_err(db)?;
        let ids: Vec<i64> = statement
            .query_map(parameters, |row| row.get(0))
            .map_err(db)?
            .collect::<Result<_, _>>()
            .map_err(db)?;
        drop(statement);
        for id in &ids {
            tx.execute("UPDATE facts SET supersedes_id=NULL WHERE supersedes_id=?1", [id])
                .map_err(db)?;
            tx.execute("DELETE FROM facts_fts WHERE fact_id=?1", [id]).map_err(db)?;
            tx.execute("DELETE FROM facts WHERE id=?1", [id]).map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(ids.len())
    }

    pub fn export(
        &self,
        workspace: &WorkspaceIdentity,
        redacted: bool,
        approval: MutationApproval,
    ) -> Result<WorkspaceExport, MemoryError> {
        self.mutate(approval)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT f.* FROM facts f WHERE workspace_digest=?1 AND state='active' AND freshness!='stale'
             AND (expires_at IS NULL OR expires_at>?2) ORDER BY namespace, normalized_key, id"
        ).map_err(db)?;
        let facts = collect_facts(
            &connection,
            &mut statement,
            params![workspace.digest(), Utc::now().to_rfc3339()],
        )?;
        let facts = facts
            .into_iter()
            .map(|fact| ExportedFact {
                namespace: fact.namespace,
                key: fact.key,
                value: (!redacted).then_some(fact.value),
                kind: fact.kind,
                authority: fact.authority,
                freshness: fact.freshness,
                provenance: fact.provenance,
                expires_at: fact.expires_at,
                content_hash: fact.content_hash,
            })
            .collect();
        Ok(WorkspaceExport {
            schema_version: SCHEMA_VERSION,
            workspace_digest: workspace.digest().into(),
            redacted,
            facts,
        })
    }

    pub fn import(
        &self,
        workspace: &WorkspaceIdentity,
        export: WorkspaceExport,
        approval: MutationApproval,
    ) -> Result<usize, MemoryError> {
        self.mutate(approval)?;
        if export.schema_version != SCHEMA_VERSION {
            return Err(MemoryError::UnsupportedExport);
        }
        if export.redacted || export.facts.iter().any(|fact| fact.value.is_none()) {
            return Err(MemoryError::RedactedImport);
        }
        if export.workspace_digest != workspace.digest() {
            return Err(MemoryError::InvalidWorkspace("export workspace mismatch"));
        }
        if export.facts.len() > self.config.quotas.max_active_facts {
            return Err(MemoryError::QuotaExceeded("import fact count"));
        }
        let mut prepared = Vec::with_capacity(export.facts.len());
        let mut keys = HashSet::new();
        let mut bytes = 0usize;
        for fact in export.facts {
            if fact.authority == FactAuthority::AgentCandidate
                || fact.freshness == FactFreshness::Stale
            {
                return Err(MemoryError::InvalidTransition);
            }
            let value = fact.value.unwrap_or_default();
            let mut new = NewWorkspaceFact {
                namespace: fact.namespace,
                key: fact.key,
                value,
                kind: fact.kind,
                // Approval establishes only a user assertion. Imported metadata
                // cannot forge local host verification authority.
                authority: FactAuthority::UserAsserted,
                freshness: FactFreshness::Current,
                provenance: FactProvenance {
                    source_kind: "workspace_memory_import".to_string(),
                    source_id: "import:pending-validation".to_string(),
                    source_revision: None,
                    source_fingerprint: None,
                    verified_at: None,
                },
                expires_at: fact.expires_at,
                relations: vec![],
            };
            let normalized = validate_fact(&new, &self.config.quotas)?;
            let expected_hash = content_hash(&normalized.0, &normalized.1, &new.value);
            if fact.content_hash != expected_hash {
                return Err(MemoryError::InvalidFact("import content hash mismatch"));
            }
            new.provenance.source_id = format!("import:{}", &expected_hash["sha256:".len()..]);
            new.provenance.source_fingerprint = Some(expected_hash);
            if !keys.insert(normalized.clone()) {
                return Err(MemoryError::Conflict);
            }
            bytes = bytes.saturating_add(new.value.len());
            prepared.push((normalized, new));
        }
        if bytes > self.config.quotas.max_active_bytes {
            return Err(MemoryError::QuotaExceeded("import active bytes"));
        }
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        ensure_workspace(&tx, workspace)?;
        enforce_quota(
            &tx,
            workspace.digest(),
            bytes,
            prepared.len(),
            bytes,
            prepared.len(),
            &self.config,
        )?;
        for ((namespace, key), fact) in &prepared {
            if active_by_key(&tx, workspace.digest(), namespace, key)?.is_some() {
                return Err(MemoryError::Conflict);
            }
            insert_fact(&tx, workspace.digest(), namespace, key, fact, None, &self.config)?;
        }
        tx.commit().map_err(db)?;
        Ok(prepared.len())
    }

    fn transition_key(
        &self,
        workspace: &WorkspaceIdentity,
        namespace: &str,
        key: &str,
        state: FactState,
        approval: MutationApproval,
    ) -> Result<(), MemoryError> {
        self.mutate(approval)?;
        let namespace = normalize_component(namespace)?;
        let key = normalize_component(key)?;
        let mut connection = self.connection()?;
        let tx =
            connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let changed = tx.execute("UPDATE facts SET state=?1, updated_at=?2 WHERE workspace_digest=?3 AND namespace=?4 AND normalized_key=?5 AND state='active'",
            params![state.as_str(), Utc::now().to_rfc3339(), workspace.digest(), namespace, key]).map_err(db)?;
        if changed == 0 {
            return Err(MemoryError::NotFound);
        }
        tx.commit().map_err(db)
    }

    fn read_by_id(
        &self,
        workspace: &WorkspaceIdentity,
        id: FactId,
    ) -> Result<Option<WorkspaceFact>, MemoryError> {
        let connection = self.connection()?;
        fact_by_id(&connection, workspace.digest(), id)
    }

    fn enabled(&self) -> Result<(), MemoryError> {
        if self.config.enabled { Ok(()) } else { Err(MemoryError::Disabled) }
    }
    fn mutate(&self, approval: MutationApproval) -> Result<(), MemoryError> {
        self.enabled()?;
        if approval != MutationApproval::Approved {
            return Err(MemoryError::ApprovalRequired);
        }
        Ok(())
    }

    fn initialize(&self) -> Result<(), MemoryError> {
        secure_parent(&self.path)?;
        let existed = self.path.exists();
        if existed && has_invalid_sqlite_header(&self.path)? {
            return Err(self.quarantine()?);
        }
        let mut connection = match Connection::open(&self.path) {
            Ok(connection) => connection,
            Err(_) if existed => return Err(self.quarantine()?),
            Err(_) => return Err(MemoryError::Database("sqlite open failed")),
        };
        if configure(&connection, self.config.busy_timeout).is_err() {
            drop(connection);
            return if existed {
                Err(self.quarantine()?)
            } else {
                Err(MemoryError::Database("sqlite configuration failed"))
            };
        }
        let quick: Result<String, _> =
            connection.query_row("PRAGMA quick_check", [], |row| row.get(0));
        if !matches!(quick.as_deref(), Ok("ok")) {
            drop(connection);
            return Err(self.quarantine()?);
        }
        migrations::migrate(&mut connection)?;
        secure_file(&self.path)?;
        Ok(())
    }

    fn connection(&self) -> Result<Connection, MemoryError> {
        let connection = Connection::open(&self.path).map_err(db)?;
        configure(&connection, self.config.busy_timeout)?;
        Ok(connection)
    }

    fn quarantine(&self) -> Result<MemoryError, MemoryError> {
        let stamp = Utc::now().timestamp_millis();
        let quarantine = self.path.with_extension(format!("sqlite3.corrupt-{stamp}"));
        if self.path.exists() {
            fs::rename(&self.path, &quarantine)
                .map_err(|_| MemoryError::Database("database quarantine failed"))?;
        }
        for suffix in ["-wal", "-shm"] {
            let sidecar = path_with_suffix(&self.path, suffix);
            if sidecar.exists() {
                let target = path_with_suffix(&quarantine, suffix);
                fs::rename(sidecar, target)
                    .map_err(|_| MemoryError::Database("database sidecar quarantine failed"))?;
            }
        }
        Ok(MemoryError::DatabaseQuarantined { path: quarantine })
    }
}

fn configure(connection: &Connection, timeout: std::time::Duration) -> Result<(), MemoryError> {
    connection.busy_timeout(timeout).map_err(db)?;
    connection.pragma_update(None, "foreign_keys", "ON").map_err(db)?;
    connection.pragma_update(None, "journal_mode", "WAL").map_err(db)?;
    Ok(())
}

fn ensure_workspace(
    tx: &Transaction<'_>,
    workspace: &WorkspaceIdentity,
) -> Result<(), MemoryError> {
    tx.execute(
        "INSERT OR IGNORE INTO workspaces(digest, canonical_root, created_at) VALUES(?1, ?2, ?3)",
        params![
            workspace.digest(),
            workspace.canonical_root().as_os_str().as_encoded_bytes(),
            Utc::now().to_rfc3339()
        ],
    )
    .map_err(db)?;
    Ok(())
}

fn insert_fact(
    tx: &Transaction<'_>,
    workspace: &str,
    namespace: &str,
    key: &str,
    fact: &NewWorkspaceFact,
    supersedes: Option<FactId>,
    config: &MemoryConfig,
) -> Result<WorkspaceFact, MemoryError> {
    let now = Utc::now();
    let expires_at = match (fact.expires_at, config.retention.default_expiry) {
        (Some(expiry), _) => Some(expiry),
        (None, Some(expiry)) => Some(now + chrono_duration(expiry)?),
        (None, None) => None,
    };
    let state = initial_state(fact.authority);
    let hash = content_hash(namespace, key, &fact.value);
    tx.execute(
        "INSERT INTO facts(workspace_digest,namespace,normalized_key,value,kind,authority,freshness,state,source_kind,source_id,
         source_revision,source_fingerprint,created_at,updated_at,verified_at,expires_at,content_hash,schema_version,supersedes_id,value_bytes)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13,?14,?15,?16,?17,?18,?19)",
        params![workspace, namespace, key, fact.value, fact.kind.as_str(), fact.authority.as_str(), fact.freshness.as_str(), state.as_str(),
            fact.provenance.source_kind, fact.provenance.source_id, fact.provenance.source_revision, fact.provenance.source_fingerprint,
            now.to_rfc3339(), fact.provenance.verified_at.map(|v| v.to_rfc3339()), expires_at.map(|v| v.to_rfc3339()), hash,
            SCHEMA_VERSION, supersedes.map(|id| id.0), fact.value.len() as i64]
    ).map_err(db)?;
    let id = FactId(tx.last_insert_rowid());
    tx.execute("INSERT INTO facts_fts(fact_id,workspace_digest,namespace,normalized_key,value) VALUES(?1,?2,?3,?4,?5)",
        params![id.0, workspace, namespace, key, fact.value]).map_err(db)?;
    for relation in &fact.relations {
        let target_workspace: Option<String> = tx
            .query_row(
                "SELECT workspace_digest FROM facts WHERE id=?1",
                [relation.target.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        if target_workspace.as_deref() != Some(workspace) {
            return Err(MemoryError::InvalidFact("relation target must exist in same workspace"));
        }
        tx.execute(
            "INSERT INTO fact_relations(fact_id,relation_kind,target_fact_id) VALUES(?1,?2,?3)",
            params![id.0, relation.kind.as_str(), relation.target.0],
        )
        .map_err(db)?;
    }
    Ok(WorkspaceFact {
        id,
        workspace_digest: workspace.into(),
        namespace: namespace.into(),
        key: key.into(),
        value: fact.value.clone(),
        kind: fact.kind,
        authority: fact.authority,
        freshness: fact.freshness,
        state,
        provenance: fact.provenance.clone(),
        created_at: now,
        updated_at: now,
        expires_at,
        content_hash: hash,
        schema_version: SCHEMA_VERSION,
        supersedes,
        relations: fact.relations.clone(),
    })
}

fn active_by_key(
    connection: &Connection,
    workspace: &str,
    namespace: &str,
    key: &str,
) -> Result<Option<WorkspaceFact>, MemoryError> {
    let mut statement = connection.prepare("SELECT f.* FROM facts f WHERE workspace_digest=?1 AND namespace=?2 AND normalized_key=?3 AND state='active' ORDER BY id DESC LIMIT 1").map_err(db)?;
    let mut facts = collect_facts(connection, &mut statement, params![workspace, namespace, key])?;
    Ok(facts.pop())
}

fn fact_by_id(
    connection: &Connection,
    workspace: &str,
    id: FactId,
) -> Result<Option<WorkspaceFact>, MemoryError> {
    let mut statement = connection
        .prepare("SELECT f.* FROM facts f WHERE workspace_digest=?1 AND id=?2")
        .map_err(db)?;
    let mut facts = collect_facts(connection, &mut statement, params![workspace, id.0])?;
    Ok(facts.pop())
}

fn collect_facts<P: rusqlite::Params>(
    connection: &Connection,
    statement: &mut rusqlite::Statement<'_>,
    parameters: P,
) -> Result<Vec<WorkspaceFact>, MemoryError> {
    let mut facts: Vec<WorkspaceFact> = statement
        .query_map(parameters, row_to_fact)
        .map_err(db)?
        .collect::<Result<_, _>>()
        .map_err(db)?;
    for fact in &mut facts {
        fact.relations = load_relations(connection, fact.id)?;
    }
    Ok(facts)
}

fn row_to_fact(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceFact> {
    let parse_time = |index| -> rusqlite::Result<DateTime<Utc>> {
        let value: String = row.get(index)?;
        DateTime::parse_from_rfc3339(&value).map(|v| v.with_timezone(&Utc)).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let optional_time = |index| -> rusqlite::Result<Option<DateTime<Utc>>> {
        let value: Option<String> = row.get(index)?;
        value
            .map(|v| DateTime::parse_from_rfc3339(&v).map(|v| v.with_timezone(&Utc)))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
    };
    Ok(WorkspaceFact {
        id: FactId(row.get("id")?),
        workspace_digest: row.get("workspace_digest")?,
        namespace: row.get("namespace")?,
        key: row.get("normalized_key")?,
        value: row.get("value")?,
        kind: FactKind::parse(&row.get::<_, String>("kind")?).map_err(enum_error)?,
        authority: FactAuthority::parse(&row.get::<_, String>("authority")?).map_err(enum_error)?,
        freshness: FactFreshness::parse(&row.get::<_, String>("freshness")?).map_err(enum_error)?,
        state: FactState::parse(&row.get::<_, String>("state")?).map_err(enum_error)?,
        provenance: FactProvenance {
            source_kind: row.get("source_kind")?,
            source_id: row.get("source_id")?,
            source_revision: row.get("source_revision")?,
            source_fingerprint: row.get("source_fingerprint")?,
            verified_at: optional_time(15)?,
        },
        created_at: parse_time(13)?,
        updated_at: parse_time(14)?,
        expires_at: optional_time(16)?,
        content_hash: row.get("content_hash")?,
        schema_version: row.get::<_, u32>("schema_version")?,
        supersedes: row.get::<_, Option<i64>>("supersedes_id")?.map(FactId),
        relations: vec![],
    })
}

fn enum_error(_: MemoryError) -> rusqlite::Error {
    rusqlite::Error::InvalidQuery
}

fn load_relations(connection: &Connection, id: FactId) -> Result<Vec<FactRelation>, MemoryError> {
    let mut statement = connection.prepare("SELECT relation_kind,target_fact_id FROM fact_relations WHERE fact_id=?1 ORDER BY relation_kind,target_fact_id").map_err(db)?;
    statement
        .query_map([id.0], |row| {
            Ok(FactRelation {
                kind: FactRelationKind::parse(&row.get::<_, String>(0)?).map_err(enum_error)?,
                target: FactId(row.get(1)?),
            })
        })
        .map_err(db)?
        .collect::<Result<_, _>>()
        .map_err(db)
}

fn recall_stage(
    connection: &Connection,
    workspace: &str,
    text: &str,
    reason: SelectionReason,
    include_stale: bool,
) -> Result<Vec<WorkspaceFact>, MemoryError> {
    let now = Utc::now().to_rfc3339();
    match reason {
        SelectionReason::ExactKey => {
            let mut statement = connection.prepare("SELECT f.* FROM facts f WHERE workspace_digest=?1 AND normalized_key=?2 AND ((state='active' AND freshness!='stale') OR (?3=1 AND state='stale' AND freshness='stale')) AND (expires_at IS NULL OR expires_at>?4) ORDER BY namespace,id").map_err(db)?;
            collect_facts(connection, &mut statement, params![workspace, text, include_stale, now])
        }
        SelectionReason::KeyPrefix => {
            let pattern = format!("{}%", escape_like(text));
            let mut statement = connection.prepare("SELECT f.* FROM facts f WHERE workspace_digest=?1 AND normalized_key LIKE ?2 ESCAPE '\\' AND normalized_key!=?3 AND ((state='active' AND freshness!='stale') OR (?4=1 AND state='stale' AND freshness='stale')) AND (expires_at IS NULL OR expires_at>?5) ORDER BY length(normalized_key),normalized_key,namespace,id").map_err(db)?;
            collect_facts(
                connection,
                &mut statement,
                params![workspace, pattern, text, include_stale, now],
            )
        }
        SelectionReason::FullText => {
            let Some(expression) = fts_expression(text) else { return Ok(vec![]) };
            let mut statement = connection.prepare("SELECT f.* FROM facts_fts x JOIN facts f ON f.id=x.fact_id WHERE x.workspace_digest=?1 AND facts_fts MATCH ?2 AND f.normalized_key!=?3 AND ((f.state='active' AND f.freshness!='stale') OR (?4=1 AND f.state='stale' AND f.freshness='stale')) AND (f.expires_at IS NULL OR f.expires_at>?5) ORDER BY bm25(facts_fts),f.normalized_key,f.namespace,f.id").map_err(db)?;
            collect_facts(
                connection,
                &mut statement,
                params![workspace, expression, text, include_stale, now],
            )
        }
    }
}

fn fts_expression(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

fn matches_query(fact: &WorkspaceFact, query: &FactQuery) -> bool {
    query
        .namespace_prefix
        .as_ref()
        .is_none_or(|prefix| fact.namespace.starts_with(&prefix.trim().to_ascii_lowercase()))
        && (query.kinds.is_empty() || query.kinds.contains(&fact.kind))
        && (query.authorities.is_empty() || query.authorities.contains(&fact.authority))
}

fn current_and_unexpired(fact: &WorkspaceFact) -> bool {
    fact.freshness != FactFreshness::Stale
        && fact.expires_at.is_none_or(|expiry| expiry > Utc::now())
}

fn bounded(mut hits: Vec<RecallHit>, limit: usize) -> RecallResult {
    let total_matches = hits.len();
    hits.truncate(limit);
    let omitted_count = total_matches.saturating_sub(hits.len());
    RecallResult { hits, total_matches, omitted_count, truncated: omitted_count > 0 }
}

fn usage(connection: &Connection, workspace: &str) -> Result<(usize, usize), MemoryError> {
    let (facts, bytes): (i64, i64) = connection.query_row("SELECT COUNT(*),COALESCE(SUM(value_bytes),0) FROM facts WHERE workspace_digest=?1 AND state='active' AND freshness!='stale' AND (expires_at IS NULL OR expires_at>?2)",
        params![workspace, Utc::now().to_rfc3339()], |row| Ok((row.get(0)?, row.get(1)?))).map_err(db)?;
    Ok((facts as usize, bytes as usize))
}

fn total_usage(connection: &Connection, workspace: &str) -> Result<(usize, usize), MemoryError> {
    let (facts, bytes): (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*),COALESCE(SUM(value_bytes),0) FROM facts WHERE workspace_digest=?1",
            [workspace],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(db)?;
    Ok((facts as usize, bytes as usize))
}

fn enforce_quota(
    connection: &Connection,
    workspace: &str,
    added_active_bytes: usize,
    added_active_facts: usize,
    added_total_bytes: usize,
    added_total_facts: usize,
    config: &MemoryConfig,
) -> Result<(), MemoryError> {
    let (active_facts, active_bytes) = usage(connection, workspace)?;
    if active_facts.saturating_add(added_active_facts) > config.quotas.max_active_facts {
        return Err(MemoryError::QuotaExceeded("active fact count"));
    }
    if active_bytes.saturating_add(added_active_bytes) > config.quotas.max_active_bytes {
        return Err(MemoryError::QuotaExceeded("active fact bytes"));
    }
    let (total_facts, total_bytes) = total_usage(connection, workspace)?;
    if total_facts.saturating_add(added_total_facts) > config.quotas.max_total_facts {
        return Err(MemoryError::QuotaExceeded("total fact count"));
    }
    if total_bytes.saturating_add(added_total_bytes) > config.quotas.max_total_bytes {
        return Err(MemoryError::QuotaExceeded("total fact bytes"));
    }
    Ok(())
}

fn ids_by_key(
    connection: &Connection,
    workspace: &str,
    namespace: &str,
    key: &str,
) -> Result<Vec<i64>, MemoryError> {
    let mut statement = connection
        .prepare(
            "SELECT id FROM facts WHERE workspace_digest=?1 AND namespace=?2 AND normalized_key=?3",
        )
        .map_err(db)?;
    statement
        .query_map(params![workspace, namespace, key], |row| row.get(0))
        .map_err(db)?
        .collect::<Result<_, _>>()
        .map_err(db)
}

fn chrono_duration(duration: std::time::Duration) -> Result<chrono::Duration, MemoryError> {
    chrono::Duration::from_std(duration)
        .map_err(|_| MemoryError::InvalidFact("retention duration is too large"))
}

fn retention_cutoff(
    now: DateTime<Utc>,
    retention: std::time::Duration,
) -> Result<DateTime<Utc>, MemoryError> {
    Ok(now - chrono_duration(retention)?)
}

fn escape_like(value: &str) -> String {
    value.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut encoded = path.as_os_str().to_os_string();
    encoded.push(suffix);
    PathBuf::from(encoded)
}

fn has_invalid_sqlite_header(path: &Path) -> Result<bool, MemoryError> {
    use std::io::Read;
    let mut file =
        fs::File::open(path).map_err(|_| MemoryError::Database("database inspection failed"))?;
    let length =
        file.metadata().map_err(|_| MemoryError::Database("database inspection failed"))?.len();
    if length == 0 {
        return Ok(false);
    }
    let mut header = [0_u8; 16];
    let read =
        file.read(&mut header).map_err(|_| MemoryError::Database("database inspection failed"))?;
    Ok(read < header.len() || &header != b"SQLite format 3\0")
}

fn secure_parent(path: &Path) -> Result<(), MemoryError> {
    let parent = path.parent().ok_or(MemoryError::Database("database path has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|_| MemoryError::Database("state directory creation failed"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| MemoryError::Database("state directory permission failed"))?;
    }
    Ok(())
}

fn secure_file(path: &Path) -> Result<(), MemoryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| MemoryError::Database("database permission failed"))?;
    }
    Ok(())
}
