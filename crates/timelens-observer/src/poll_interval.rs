use std::time::{Duration, Instant};

/// Poll immediately until the first check finishes, without subtracting from the
/// Windows performance counter, which can still be near zero just after boot.
pub struct PollInterval {
    interval: Duration,
    last: Option<Instant>,
}

impl PollInterval {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.last.map_or(Duration::ZERO, |last| {
            self.interval
                .saturating_sub(now.saturating_duration_since(last))
        })
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.remaining(now).is_zero()
    }

    pub fn mark(&mut self, now: Instant) {
        self.last = Some(now);
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near_clock_origin() -> Instant {
        let now = Instant::now();
        let (mut low, mut high) = (0_u64, u64::MAX);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if now.checked_sub(Duration::from_nanos(middle)).is_some() {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        now.checked_sub(Duration::from_nanos(low)).unwrap()
    }

    #[test]
    fn first_poll_works_before_the_interval_has_elapsed_since_boot() {
        let boot = near_clock_origin();
        for seconds in [1, 2, 20, 30] {
            let interval = Duration::from_secs(seconds);
            assert!(boot.checked_sub(interval).is_none());
            let mut timer = PollInterval::new(interval);
            assert!(timer.is_due(boot));
            timer.mark(boot);
            assert!(!timer.is_due(boot));
            assert_eq!(timer.remaining(boot), interval);
            assert!(timer.is_due(boot + interval));
        }
    }

    #[test]
    fn reset_requests_an_immediate_poll_and_completed_checks_keep_the_interval() {
        let now = Instant::now();
        let mut timer = PollInterval::new(Duration::from_secs(15));
        timer.mark(now);
        assert_eq!(
            timer.remaining(now + Duration::from_secs(14)),
            Duration::from_secs(1)
        );
        assert!(timer.is_due(now + Duration::from_secs(15)));
        timer.mark(now + Duration::from_secs(15));
        assert!(!timer.is_due(now + Duration::from_secs(16)));
        timer.reset();
        assert!(timer.is_due(now + Duration::from_secs(16)));
    }
}
