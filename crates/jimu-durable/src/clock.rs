use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Injectable monotonic/wall clock abstraction (milliseconds since Unix epoch).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Default wall-clock implementation backed by [`SystemTime::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Deterministic controllable clock for zero-sleep timer and timeout testing.
#[derive(Debug)]
pub struct ManualClock {
    now_ms: AtomicU64,
}

impl ManualClock {
    pub fn new(initial_ms: u64) -> Self {
        Self {
            now_ms: AtomicU64::new(initial_ms),
        }
    }

    pub fn set(&self, ms: u64) {
        self.now_ms.store(ms, Ordering::SeqCst);
    }

    pub fn advance(&self, delta_ms: u64) -> u64 {
        let prev = self.now_ms.fetch_add(delta_ms, Ordering::SeqCst);
        prev.saturating_add(delta_ms)
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

/// Retry policy for transient LLM transport failures during `Generating`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts allowed per generation turn (minimum effective value is 1).
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3 }
    }
}

impl RetryPolicy {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
        }
    }

    pub fn effective_max_attempts(&self) -> u32 {
        self.max_attempts.max(1)
    }
}
