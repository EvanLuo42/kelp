//! Wall-clock time behind a trait so that it can be controlled in tests.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub type Millis = u64;

/// A source of wall-clock time.
///
/// Components that record or compare timestamps take a `Clock` instead of
/// calling [`SystemTime::now`] directly, so tests can substitute a manual
/// clock.
pub trait Clock: Send + Sync {
    /// Returns the current time in milliseconds since the Unix epoch.
    fn now(&self) -> Millis;
}

/// The operating system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as Millis)
    }
}
