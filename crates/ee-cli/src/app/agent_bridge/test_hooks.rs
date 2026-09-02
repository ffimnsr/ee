use std::fmt;

use crate::app::App;

type WriteVerificationTestHook = Box<dyn FnOnce(&mut App) + Send>;

#[derive(Default)]
pub(crate) struct WriteVerificationTestHooks {
    pre_verification: Option<WriteVerificationTestHook>,
    post_write: Option<WriteVerificationTestHook>,
}

impl WriteVerificationTestHooks {
    pub(super) fn set_pre_verification(&mut self, hook: WriteVerificationTestHook) {
        self.pre_verification = Some(hook);
    }

    pub(super) fn take_pre_verification(&mut self) -> Option<WriteVerificationTestHook> {
        self.pre_verification.take()
    }

    pub(super) fn set_post_write(&mut self, hook: WriteVerificationTestHook) {
        self.post_write = Some(hook);
    }

    pub(super) fn take_post_write(&mut self) -> Option<WriteVerificationTestHook> {
        self.post_write.take()
    }
}

impl fmt::Debug for WriteVerificationTestHooks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WriteVerificationTestHooks")
            .field("pre_verification_pending", &self.pre_verification.is_some())
            .field("post_write_pending", &self.post_write.is_some())
            .finish()
    }
}
