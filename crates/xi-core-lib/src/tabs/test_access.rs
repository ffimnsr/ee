//! `impl CoreState` methods: test_access.
use super::*;

impl CoreState {
    pub fn _test_open_editors(&self) -> Vec<BufferId> {
        self.editors.keys().cloned().collect()
    }

    pub fn _test_open_views(&self) -> Vec<ViewId> {
        self.views.keys().cloned().collect()
    }
}
