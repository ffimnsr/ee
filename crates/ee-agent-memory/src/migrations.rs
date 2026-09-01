use rusqlite::{Connection, Transaction};

use crate::MemoryError;

pub(crate) const DATABASE_VERSION: i64 = 2;

pub(crate) fn migrate(connection: &mut Connection) -> Result<(), MemoryError> {
    let current: i64 =
        connection.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(db)?;
    if current > DATABASE_VERSION {
        return Err(MemoryError::Database("database schema is newer than this crate"));
    }
    let transaction = connection.transaction().map_err(db)?;
    if current < 1 {
        migration_one(&transaction)?;
    }
    if current < 2 {
        migration_two(&transaction)?;
    }
    transaction.pragma_update(None, "user_version", DATABASE_VERSION).map_err(db)?;
    transaction.commit().map_err(db)
}

fn migration_one(tx: &Transaction<'_>) -> Result<(), MemoryError> {
    tx.execute_batch(
        "CREATE TABLE workspaces (
            digest TEXT PRIMARY KEY NOT NULL,
            canonical_root BLOB NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE facts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_digest TEXT NOT NULL REFERENCES workspaces(digest) ON DELETE CASCADE,
            namespace TEXT NOT NULL,
            normalized_key TEXT NOT NULL,
            value TEXT NOT NULL,
            kind TEXT NOT NULL,
            authority TEXT NOT NULL,
            freshness TEXT NOT NULL,
            state TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            source_id TEXT NOT NULL,
            source_revision TEXT,
            source_fingerprint TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            verified_at TEXT,
            expires_at TEXT,
            content_hash TEXT NOT NULL,
            schema_version INTEGER NOT NULL,
            supersedes_id INTEGER REFERENCES facts(id),
            value_bytes INTEGER NOT NULL
        );
        CREATE TABLE fact_relations (
            fact_id INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
            relation_kind TEXT NOT NULL,
            target_fact_id INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
            PRIMARY KEY (fact_id, relation_kind, target_fact_id)
        );
        CREATE VIRTUAL TABLE facts_fts USING fts5(
            fact_id UNINDEXED, workspace_digest UNINDEXED, namespace, normalized_key, value,
            tokenize='unicode61 remove_diacritics 2'
        );
        CREATE INDEX facts_workspace_key ON facts(workspace_digest, namespace, normalized_key, state);
        CREATE INDEX facts_source ON facts(workspace_digest, source_kind, source_id, state);
        CREATE INDEX facts_retention ON facts(workspace_digest, state, updated_at);"
    ).map_err(db)
}

fn migration_two(tx: &Transaction<'_>) -> Result<(), MemoryError> {
    tx.execute_batch("CREATE INDEX facts_active_expiry ON facts(workspace_digest, state, freshness, expires_at);")
        .map_err(db)
}

pub(crate) fn db(_: rusqlite::Error) -> MemoryError {
    MemoryError::Database("sqlite operation failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_versioned_and_idempotent() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        migrate(&mut connection).unwrap();
        let version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, DATABASE_VERSION);
    }
}
