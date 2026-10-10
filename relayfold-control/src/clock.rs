//! Injected time. Every time-dependent decision in this crate (timestamp skew, nonce-window
//! pruning, heartbeat TTL) reads a [`Clock`], never the wall clock directly, so the tests can
//! step time deterministically.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now_unix(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// A clock that only moves when told to. Clones share the same instant.
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    pub fn new(start_unix: i64) -> Self {
        ManualClock(Arc::new(AtomicI64::new(start_unix)))
    }

    pub fn set(&self, unix: i64) {
        self.0.store(unix, Ordering::SeqCst);
    }

    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
