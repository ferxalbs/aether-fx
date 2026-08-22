use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{CoreError, CoreResult};

/// A cheap, Tokio-independent cancellation signal shared with blocking work.
#[derive(Clone, Debug, Default)]
pub struct CancellationFlag {
    cancelled: Arc<AtomicBool>,
}

impl CancellationFlag {
    /// Construct a fresh non-cancelled flag.
    pub fn new() -> Self {
        Self { cancelled: Arc::new(AtomicBool::new(false)) }
    }

    /// Request cooperative cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Convert the signal into the shared typed error.
    pub fn check(&self) -> CoreResult<()> {
        if self.is_cancelled() { Err(CoreError::Cancelled) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_and_typed() {
        let flag = CancellationFlag::new();
        let clone = flag.clone();
        assert!(!clone.is_cancelled());
        flag.cancel();
        assert!(clone.is_cancelled());
        assert_eq!(clone.check(), Err(CoreError::Cancelled));
    }
}
