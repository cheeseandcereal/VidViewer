//! The `Clock` trait provides an injectable source of wall-clock time.
//!
//! Tests use `FixedClock` for deterministic timestamps. Production code uses `SystemClock`.

use std::sync::Arc;

use chrono::{DateTime, Utc};

/// A source of wall-clock time. All code that needs "now" should depend on this.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

/// Production implementation: calls `Utc::now()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A thread-safe, cloneable handle to a `Clock` implementation.
pub type ClockRef = Arc<dyn Clock>;

pub fn system() -> ClockRef {
    Arc::new(SystemClock)
}

#[cfg(test)]
pub mod test_clock {
    use std::sync::Mutex;

    use chrono::{DateTime, TimeZone, Utc};

    use super::Clock;

    /// A clock that returns a configurable fixed time.
    #[derive(Debug)]
    pub struct FixedClock {
        now: Mutex<DateTime<Utc>>,
    }

    impl FixedClock {
        pub fn at(dt: DateTime<Utc>) -> Self {
            Self {
                now: Mutex::new(dt),
            }
        }

        pub fn epoch() -> Self {
            Self::at(Utc.timestamp_opt(0, 0).unwrap())
        }

        pub fn advance(&self, seconds: i64) {
            let mut g = self.now.lock().unwrap();
            *g += chrono::Duration::seconds(seconds);
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            *self.now.lock().unwrap()
        }
    }
}
