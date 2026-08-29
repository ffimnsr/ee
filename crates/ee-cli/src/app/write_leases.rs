// Copyright 2026 The ee authors. All rights reserved.

//! Session-attributed write leases for concurrent top-level agent turns.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Stable identity for one held write lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WriteLeaseId(u64);

/// Connection, session, and turn that owns one write lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteLeaseOwner {
    pub(crate) connection_id: String,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
}

/// Conflict returned when another owner already holds an overlapping scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteLeaseConflict {
    pub(crate) owner: WriteLeaseOwner,
    pub(crate) path: PathBuf,
}

impl fmt::Display for WriteLeaseConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "write scope conflict with connection {} session {} turn {} at {}",
            self.owner.connection_id,
            self.owner.session_id,
            self.owner.turn_id,
            self.path.display()
        )
    }
}

#[derive(Debug)]
struct WriteLease {
    owner: WriteLeaseOwner,
    scopes: Vec<PathBuf>,
    revisions: BTreeMap<PathBuf, String>,
}

/// Pane-wide lease table. Paths entering this type must already use canonical
/// workspace identity supplied by existing bridge/filesystem APIs.
#[derive(Debug, Default)]
pub(crate) struct WriteLeaseCoordinator {
    next_id: u64,
    held: BTreeMap<WriteLeaseId, WriteLease>,
}

impl WriteLeaseCoordinator {
    pub(crate) fn acquire(
        &mut self,
        owner: WriteLeaseOwner,
        mut scopes: Vec<PathBuf>,
        revisions: BTreeMap<PathBuf, String>,
    ) -> Result<WriteLeaseId, WriteLeaseConflict> {
        scopes.sort();
        scopes.dedup();
        debug_assert!(scopes.iter().all(|path| path.is_absolute()));

        for lease in self.held.values() {
            if let Some(path) = lease
                .scopes
                .iter()
                .find(|held| scopes.iter().any(|candidate| paths_overlap(held, candidate)))
            {
                return Err(WriteLeaseConflict { owner: lease.owner.clone(), path: path.clone() });
            }
        }

        let id = WriteLeaseId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.held.insert(id, WriteLease { owner, scopes, revisions });
        Ok(id)
    }

    pub(crate) fn validate(
        &self,
        id: WriteLeaseId,
        owner: &WriteLeaseOwner,
        revisions: &BTreeMap<PathBuf, String>,
    ) -> Result<(), &'static str> {
        let lease = self.held.get(&id).ok_or("write lease is no longer active")?;
        if &lease.owner != owner {
            return Err("write lease owner does not match approval owner");
        }
        if &lease.revisions != revisions {
            return Err("write scope changed after lease acquisition; re-read and retry");
        }
        Ok(())
    }

    pub(crate) fn scopes(&self, id: WriteLeaseId) -> Option<&[PathBuf]> {
        self.held.get(&id).map(|lease| lease.scopes.as_slice())
    }

    pub(crate) fn release(&mut self, id: WriteLeaseId) -> bool {
        self.held.remove(&id).is_some()
    }

    pub(crate) fn release_session(&mut self, session_id: &str) {
        self.held.retain(|_, lease| lease.owner.session_id != session_id);
    }

    pub(crate) fn release_connection(&mut self, connection_id: &str) {
        self.held.retain(|_, lease| lease.owner.connection_id != connection_id);
    }

    pub(crate) fn clear(&mut self) {
        self.held.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(session: &str) -> WriteLeaseOwner {
        WriteLeaseOwner {
            connection_id: String::from("agent-a"),
            session_id: session.to_string(),
            turn_id: format!("turn-{session}"),
        }
    }

    fn revisions(path: &str, revision: &str) -> BTreeMap<PathBuf, String> {
        BTreeMap::from([(PathBuf::from(path), revision.to_string())])
    }

    #[test]
    fn disjoint_sessions_hold_leases_concurrently() {
        let mut leases = WriteLeaseCoordinator::default();
        leases
            .acquire(owner("s1"), vec![PathBuf::from("/repo/a.rs")], revisions("/repo/a.rs", "1"))
            .expect("first lease");
        leases
            .acquire(owner("s2"), vec![PathBuf::from("/repo/b.rs")], revisions("/repo/b.rs", "1"))
            .expect("disjoint lease");
        assert_eq!(leases.len(), 2);
    }

    #[test]
    fn overlap_reports_exact_owner_and_ancestor_scope() {
        let mut leases = WriteLeaseCoordinator::default();
        leases
            .acquire(owner("s1"), vec![PathBuf::from("/repo/src")], revisions("/repo/src", "1"))
            .expect("first lease");
        let conflict = leases
            .acquire(
                owner("s2"),
                vec![PathBuf::from("/repo/src/lib.rs")],
                revisions("/repo/src/lib.rs", "1"),
            )
            .expect_err("overlap rejected");
        assert_eq!(conflict.owner.session_id, "s1");
        assert_eq!(conflict.path, PathBuf::from("/repo/src"));
    }

    #[test]
    fn validation_checks_owner_and_acquire_revision() {
        let mut leases = WriteLeaseCoordinator::default();
        let initial = revisions("/repo/a.rs", "1");
        let id = leases
            .acquire(owner("s1"), vec![PathBuf::from("/repo/a.rs")], initial.clone())
            .expect("lease");
        assert_eq!(
            leases.validate(id, &owner("s2"), &initial),
            Err("write lease owner does not match approval owner")
        );
        assert_eq!(
            leases.validate(id, &owner("s1"), &revisions("/repo/a.rs", "2")),
            Err("write scope changed after lease acquisition; re-read and retry")
        );
        assert!(leases.validate(id, &owner("s1"), &initial).is_ok());
    }

    #[test]
    fn session_connection_and_shutdown_release_only_owned_leases() {
        let mut leases = WriteLeaseCoordinator::default();
        leases
            .acquire(owner("s1"), vec![PathBuf::from("/repo/a")], revisions("/repo/a", "1"))
            .expect("s1");
        let mut other = owner("s2");
        other.connection_id = String::from("agent-b");
        leases
            .acquire(other, vec![PathBuf::from("/repo/b")], revisions("/repo/b", "1"))
            .expect("s2");
        leases.release_session("s1");
        assert_eq!(leases.len(), 1);
        leases.release_connection("agent-b");
        assert_eq!(leases.len(), 0);
        leases.clear();
        assert_eq!(leases.len(), 0);
    }
}
