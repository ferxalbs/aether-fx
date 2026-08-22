use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use aether_core::{CoreError, CoreResult};
use tokio::sync::Notify;

/// Cheap, clonable cancellation signal shared by an agent turn and its tools.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
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
        Self { cancelled: Arc::new(AtomicBool::new(false)), notify: Arc::new(Notify::new()) }
    }

    /// Mark the operation cancelled.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Check whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
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
        if self.is_cancelled() { Err(CoreError::Cancelled) } else { Ok(()) }
    }
}
