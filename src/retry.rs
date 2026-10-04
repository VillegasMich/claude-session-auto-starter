//! Exponential backoff for failed starts: 1, 2, 4, ... minutes, capped.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

impl Backoff {
    /// Delay after `failures` consecutive failures (1-based): `initial * 2^(failures-1)`,
    /// capped at `max`.
    pub fn delay(&self, failures: u32) -> Duration {
        let factor = 2u32.saturating_pow(failures.saturating_sub(1));
        self.initial.saturating_mul(factor).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_and_caps() {
        let b = Backoff {
            initial: Duration::from_secs(60),
            max: Duration::from_secs(300),
        };
        let delays: Vec<u64> = (1..=5).map(|n| b.delay(n).as_secs()).collect();
        assert_eq!(delays, [60, 120, 240, 300, 300]);
        assert_eq!(b.delay(0), Duration::from_secs(60));
        assert_eq!(b.delay(u32::MAX), Duration::from_secs(300));
    }
}
