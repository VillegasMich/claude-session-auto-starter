//! Wall clock + interruptible sleep, abstracted so scheduling logic can be tested with a fake.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

/// Longest single sleep. Long waits are split into chunks of this size and the wall clock is
/// re-read between them, so host suspend or clock jumps can't make the service miss a window.
pub const MAX_SLEEP_CHUNK: Duration = Duration::from_secs(60);

pub trait Clock {
    /// Current time, always UTC.
    fn now(&self) -> DateTime<Utc>;

    /// Sleeps for `duration`. Returns `false` if the sleep was cut short by a shutdown request.
    fn sleep(&self, duration: Duration) -> bool;

    fn shutdown_requested(&self) -> bool;
}

/// Sleeps until the wall clock reaches `target`, in chunks of at most [`MAX_SLEEP_CHUNK`].
/// Returns `false` on shutdown.
pub fn sleep_until(clock: &dyn Clock, target: DateTime<Utc>) -> bool {
    loop {
        if clock.shutdown_requested() {
            return false;
        }
        let Ok(remaining) = (target - clock.now()).to_std() else {
            return true; // target is in the past
        };
        if remaining.is_zero() {
            return true;
        }
        if !clock.sleep(remaining.min(MAX_SLEEP_CHUNK)) {
            return false;
        }
    }
}

/// Real clock. Sleeps in short ticks so `SIGTERM` is honored within a fraction of a second.
#[derive(Debug, Clone)]
pub struct SystemClock {
    shutdown: Arc<AtomicBool>,
}

const TICK: Duration = Duration::from_millis(250);

impl SystemClock {
    pub fn new(shutdown: Arc<AtomicBool>) -> Self {
        Self { shutdown }
    }

    /// The shared flag set by the signal handlers.
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }
}

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.shutdown_requested() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            std::thread::sleep(remaining.min(TICK));
        }
    }

    fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
pub mod testing {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// Deterministic clock: `sleep` advances time instantly. Optionally simulates a shutdown
    /// request after a given number of sleeps.
    pub struct FakeClock {
        now: Cell<DateTime<Utc>>,
        sleeps: RefCell<Vec<Duration>>,
        interrupt_after: Cell<Option<usize>>,
        shutdown: Cell<bool>,
    }

    impl FakeClock {
        pub fn at(rfc3339: &str) -> Self {
            Self {
                now: Cell::new(DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc()),
                sleeps: RefCell::default(),
                interrupt_after: Cell::new(None),
                shutdown: Cell::new(false),
            }
        }

        /// The `n + 1`-th sleep call reports a shutdown.
        pub fn interrupt_after(self, n: usize) -> Self {
            self.interrupt_after.set(Some(n));
            self
        }

        pub fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.borrow().clone()
        }

        /// Moves time without recording a sleep, e.g. to simulate host suspend.
        pub fn jump(&self, by: chrono::TimeDelta) {
            self.now.set(self.now.get() + by);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            self.now.get()
        }

        fn sleep(&self, duration: Duration) -> bool {
            if self.shutdown.get() || self.interrupt_after.get() == Some(self.sleeps.borrow().len())
            {
                self.shutdown.set(true);
                return false;
            }
            self.sleeps.borrow_mut().push(duration);
            self.now
                .set(self.now.get() + chrono::TimeDelta::from_std(duration).unwrap());
            true
        }

        fn shutdown_requested(&self) -> bool {
            self.shutdown.get()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeClock;
    use super::*;

    #[test]
    fn sleep_until_uses_bounded_chunks() {
        let clock = FakeClock::at("2026-10-04T12:00:00Z");
        let target = clock.now() + chrono::TimeDelta::seconds(150);
        assert!(sleep_until(&clock, target));
        assert_eq!(
            clock.sleeps(),
            [60, 60, 30].map(Duration::from_secs).to_vec()
        );
        assert_eq!(clock.now(), target);
    }

    #[test]
    fn sleep_until_past_target_returns_immediately() {
        let clock = FakeClock::at("2026-10-04T12:00:00Z");
        assert!(sleep_until(
            &clock,
            clock.now() - chrono::TimeDelta::hours(1)
        ));
        assert!(clock.sleeps().is_empty());
    }

    #[test]
    fn sleep_until_notices_clock_jumps() {
        let clock = FakeClock::at("2026-10-04T12:00:00Z");
        let target = clock.now() + chrono::TimeDelta::hours(5);
        clock.jump(chrono::TimeDelta::hours(6)); // host was suspended
        assert!(sleep_until(&clock, target));
        assert!(clock.sleeps().is_empty());
    }

    #[test]
    fn sleep_until_stops_on_shutdown() {
        let clock = FakeClock::at("2026-10-04T12:00:00Z").interrupt_after(1);
        assert!(!sleep_until(
            &clock,
            clock.now() + chrono::TimeDelta::hours(1)
        ));
        assert_eq!(clock.sleeps().len(), 1);
    }
}
