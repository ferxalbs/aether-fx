use std::sync::Arc;

use aether_core::{CancellationFlag, CoreResult};
use tokio::sync::Notify;

/// Cheap, clonable cancellation signal shared by an agent turn and its tools.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    flag: CancellationFlag,
    notify: Arc<Notify>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Construct a fresh non-cancelled token.
    pub fn new() -> Self {
        Self { flag: CancellationFlag::new(), notify: Arc::new(Notify::new()) }
    }

    /// Mark the operation cancelled.
    pub fn cancel(&self) {
        self.flag.cancel();
        self.notify.notify_waiters();
    }

    /// Check whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.flag.is_cancelled()
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }

    /// Convert the signal into the shared typed error.
    pub fn check(&self) -> CoreResult<()> {
        self.flag.check()
    }

    /// Return the std-only signal shared with blocking tool work.
    pub fn flag(&self) -> CancellationFlag {
        self.flag.clone()
    }
}
