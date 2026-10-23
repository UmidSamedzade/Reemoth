use std::time::{Duration, Instant};

/// Host-side back-off after failed password attempts.
///
/// The first few failures are free (typos); after that each failure doubles
/// the wait before the next attempt is even evaluated.
#[derive(Debug, Default)]
pub struct Lockout {
    failures: u32,
    locked_until: Option<Instant>,
}

const FREE_ATTEMPTS: u32 = 5;
const BASE_DELAY: Duration = Duration::from_secs(5);
const MAX_DELAY: Duration = Duration::from_secs(15 * 60);

impl Lockout {
    /// `Some(remaining)` while attempts must be refused.
    pub fn check(&self, now: Instant) -> Option<Duration> {
        self.locked_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    pub fn record_failure(&mut self, now: Instant) {
        self.failures += 1;
        if self.failures > FREE_ATTEMPTS {
            let exp = (self.failures - FREE_ATTEMPTS - 1).min(16);
            let delay = BASE_DELAY.saturating_mul(1 << exp).min(MAX_DELAY);
            self.locked_until = Some(now + delay);
        }
    }

    pub fn record_success(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_after_free_attempts() {
        let t = Instant::now();
        let mut l = Lockout::default();
        for _ in 0..FREE_ATTEMPTS {
            l.record_failure(t);
            assert_eq!(l.check(t), None);
        }
        l.record_failure(t);
        assert_eq!(l.check(t), Some(BASE_DELAY));
        l.record_failure(t);
        assert_eq!(l.check(t), Some(BASE_DELAY * 2));
        assert_eq!(l.check(t + BASE_DELAY * 2), None);
        for _ in 0..30 {
            l.record_failure(t);
        }
        assert_eq!(l.check(t), Some(MAX_DELAY));
        l.record_success();
        assert_eq!(l.check(t), None);
    }
}
