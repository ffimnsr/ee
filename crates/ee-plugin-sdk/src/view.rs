//! Per-view cached state.

use ee_plugin_api::model::{BufferId, SelectionRange, ViewId};

/// Revision-checked view selections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewState {
    view_id: ViewId,
    buffer_id: BufferId,
    rev: u64,
    selections: Vec<SelectionRange>,
}

impl ViewState {
    /// Create view state at a document revision.
    pub fn new(view_id: ViewId, buffer_id: BufferId, rev: u64) -> Self {
        Self { view_id, buffer_id, rev, selections: Vec::new() }
    }

    pub fn view_id(&self) -> ViewId {
        self.view_id
    }

    pub fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn selections(&self) -> &[SelectionRange] {
        &self.selections
    }

    /// Replace selections when the update is not older than the cached revision.
    pub fn apply_selection_update(
        &mut self,
        rev: u64,
        selections: Vec<SelectionRange>,
    ) -> Result<(), SelectionError> {
        if rev < self.rev {
            return Err(SelectionError::Stale { current: self.rev, found: rev });
        }
        self.rev = rev;
        self.selections = selections;
        Ok(())
    }
}

/// Failure when applying view state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    #[error("stale selection revision: current {current}, found {found}")]
    Stale { current: u64, found: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_update_is_revision_checked() {
        let mut state = ViewState::new(ViewId(1), BufferId(2), 5);
        state.apply_selection_update(6, vec![SelectionRange { start: 1, end: 3 }]).unwrap();
        assert_eq!(state.rev(), 6);
        assert_eq!(state.selections()[0].end, 3);

        assert_eq!(
            state.apply_selection_update(5, vec![]).unwrap_err(),
            SelectionError::Stale { current: 6, found: 5 }
        );
    }
}
