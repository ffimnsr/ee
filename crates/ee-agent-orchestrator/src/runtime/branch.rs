//! `impl OrchestratorRuntime` methods: checkpoint branching.
use super::*;

impl OrchestratorRuntime {
    /// Branches a new session from a stored checkpoint.
    ///
    /// The checkpoint must exist for `session_id`. When the append-only session
    /// log holds records for the session, the exact log prefix up to the
    /// checkpoint is replayed into the new session. Without a log the branch is
    /// reported as `conversation_replayed = false`; callers may still adopt
    /// task, memory, and budget state from the checkpoint.
    pub fn branch_from_checkpoint(
        &self,
        session_id: &str,
        checkpoint_seq: u64,
    ) -> Result<crate::session_log::BranchOutcome, OrchestratorError> {
        let expected = format!("{session_id}-{checkpoint_seq:010}");
        let checkpoints = self.checkpoints.list(session_id)?;
        if !checkpoints.iter().any(|meta| meta.id == expected) {
            return Err(OrchestratorError::InvalidState(format!(
                "checkpoint {expected} was not found for session {session_id}"
            )));
        }
        let Some(store) = &self.session_logs else {
            return Ok(crate::session_log::BranchOutcome::conversation_unreplayed(
                format!("{session_id}~branch{checkpoint_seq}"),
                session_id,
                checkpoint_seq,
            ));
        };
        store.log_for(session_id).branch_from(checkpoint_seq)
    }
}
